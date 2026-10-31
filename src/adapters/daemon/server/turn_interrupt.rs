//! Story 21.2 A4 — the attached stop capability for the ordinary
//! terminal-origin daemon turn, and the ordinary turn driver it hangs off.
//!
//! Every turn the daemon drives gets a server-minted [`TurnGeneration`] held in
//! the [`ActiveTurnSlot`]. A terminal-origin turn also retains its existing
//! [`CancellationToken`] there, and the generation is projected to the
//! trusted-local writer ([`DaemonFrame::ActiveTurn`]), including on attach
//! during an active turn. The writer echoes it in [`ClientFrame::InterruptTurn`].
//!
//! # The one linearization point
//!
//! The slot lives **inside** [`ConnRegistry`], so the registry mutex is the
//! existing admission fence (writer grant, registration and removal all run
//! under it). One synchronous method, [`ConnRegistry::interrupt_terminal_turn`],
//! performs authorization (this connection is the registered, live,
//! trusted-local read-write writer), the generation match and the token firing
//! under that single guard. A writer handoff, a connection removal or a slot
//! replacement therefore lands entirely before or entirely after the whole
//! decision; an atomic writer-id check followed later by `cancel()` does not
//! exist. The generation is only a stale-target fence, never authorization.
//!
//! # Lock order
//!
//! `turn_serial` → `registry`. [`AttachServer::drive_user_turn_inner`] takes
//! `turn_serial` and then the registry briefly to begin/end the slot; the
//! reader-side [`AttachServer::answer_interrupt`] takes the registry alone, so
//! it is never queued behind the operator FIFO or the turn mutex. Nothing takes
//! `turn_serial` while holding the registry. Every method that runs under the
//! registry guard here is synchronous: no `.await`, I/O or blocking send is
//! performed while the fence is held (frame writes are `try_send`).
//!
//! The token firing only requests cancellation. The turn driver races the same
//! token against the turn task, aborts a task whose provider stream never
//! yields, and emits the ordered `Cancelled` terminal chunk, so the normal
//! event projection and persistence fold run exactly as for any cancelled turn.
use super::*;
use crate::adapters::daemon::protocol::{InterruptOutcome, InterruptRefusal, TurnGeneration};

/// What kind of turn holds the slot. Only a terminal-origin turn is
/// interruptible from an attachment; its token never leaves the slot except
/// into the turn driver that created it.
enum TurnKind {
    Terminal {
        cancel: CancellationToken,
        interrupted: bool,
    },
    /// Channel-origin turns are tracked so a stale or foreign generation can
    /// be refused as wrong-origin, but their cancellation is never exposed.
    Other,
}

struct ActiveTurn {
    generation: TurnGeneration,
    kind: TurnKind,
}

/// The active-turn slot. Mutated only through [`ConnRegistry`] methods, i.e.
/// only under the registry fence.
#[derive(Default)]
pub(super) struct ActiveTurnSlot {
    minted: u64,
    current: Option<ActiveTurn>,
}

impl ActiveTurnSlot {
    fn begin(&mut self, cancel: Option<&CancellationToken>) -> TurnGeneration {
        self.minted += 1;
        let generation = TurnGeneration(self.minted);
        let kind = match cancel {
            Some(cancel) => TurnKind::Terminal {
                cancel: cancel.clone(),
                interrupted: false,
            },
            None => TurnKind::Other,
        };
        self.current = Some(ActiveTurn { generation, kind });
        generation
    }

    /// Clear only if the slot still holds `generation`. Returns whether the
    /// cleared turn was terminal-origin.
    fn end(&mut self, generation: TurnGeneration) -> Option<bool> {
        let current = self.current.as_ref()?;
        if current.generation != generation {
            return None;
        }
        let terminal = matches!(current.kind, TurnKind::Terminal { .. });
        self.current = None;
        Some(terminal)
    }

    fn terminal_generation(&self) -> Option<TurnGeneration> {
        self.current
            .as_ref()
            .filter(|turn| matches!(turn.kind, TurnKind::Terminal { .. }))
            .map(|turn| turn.generation)
    }

    fn interrupt(&mut self, generation: TurnGeneration) -> InterruptOutcome {
        let refuse = |reason| InterruptOutcome::Refused { reason };
        let Some(turn) = self.current.as_mut() else {
            return refuse(InterruptRefusal::NoActiveTurn);
        };
        if turn.generation != generation {
            return refuse(InterruptRefusal::Stale);
        }
        match &mut turn.kind {
            TurnKind::Other => refuse(InterruptRefusal::WrongOrigin),
            TurnKind::Terminal { interrupted, .. } if *interrupted => {
                refuse(InterruptRefusal::AlreadyInterrupted)
            }
            TurnKind::Terminal {
                cancel,
                interrupted,
            } => {
                *interrupted = true;
                cancel.cancel();
                InterruptOutcome::Interrupted
            }
        }
    }
}

impl ConnRegistry {
    /// The connection that may hold the stop capability: registered, granted
    /// read-write, trusted-local and not already closing.
    fn interrupt_writer(&self) -> Option<&Conn> {
        self.approval_writer()
    }

    /// Queue `frame` to the current writer without waiting. A full or closed
    /// queue drops the connection exactly as [`AttachServer::fanout`] does.
    fn project_to_writer(&mut self, frame: DaemonFrame) {
        let Some(writer) = self.interrupt_writer() else {
            return;
        };
        let id = writer.id;
        if writer.tx.try_send(frame).is_err() {
            tracing::info!(
                conn = id,
                "daemon: dropping slow/closed attach connection (turn generation)"
            );
            self.remove(id);
        }
    }

    /// Begin the slot for one turn. `cancel` is `Some` only for a
    /// terminal-origin turn: the slot retains that token for the turn's
    /// lifetime and the generation is projected to the writer.
    pub(super) fn begin_turn(&mut self, cancel: Option<&CancellationToken>) -> TurnGeneration {
        let generation = self.active_turn.begin(cancel);
        if cancel.is_some() {
            self.project_to_writer(DaemonFrame::ActiveTurn {
                generation: Some(generation),
            });
        }
        generation
    }

    /// End the slot if it still holds `generation`, telling the writer that no
    /// terminal turn is live any more.
    pub(super) fn end_turn(&mut self, generation: TurnGeneration) {
        if self.active_turn.end(generation) == Some(true) {
            self.project_to_writer(DaemonFrame::ActiveTurn { generation: None });
        }
    }

    /// A freshly registered writer learns the live terminal turn at once.
    /// Runs under the same guard that registered it, so it cannot miss a turn
    /// that begins concurrently: that turn's projection finds the writer.
    pub(super) fn project_active_turn_to(&mut self, conn_id: u64) {
        let Some(generation) = self.active_turn.terminal_generation() else {
            return;
        };
        if self.interrupt_writer().map(|conn| conn.id) == Some(conn_id) {
            self.project_to_writer(DaemonFrame::ActiveTurn {
                generation: Some(generation),
            });
        }
    }

    /// Authorization, generation match and token firing in one synchronous
    /// step. `current_writer` is the id of the most recently granted writer,
    /// read by the caller under this same guard.
    pub(super) fn interrupt_terminal_turn(
        &mut self,
        conn_id: u64,
        current_writer: u64,
        generation: TurnGeneration,
    ) -> InterruptOutcome {
        let refuse = |reason| InterruptOutcome::Refused { reason };
        let Some(conn) = self.conns.iter().find(|conn| conn.id == conn_id) else {
            return refuse(InterruptRefusal::NotWriter);
        };
        if conn.tier == ConnectionTier::Peer {
            return refuse(InterruptRefusal::PeerTier);
        }
        if conn.mode != AttachMode::ReadWrite {
            return refuse(InterruptRefusal::ReadOnly);
        }
        if current_writer != conn_id || self.interrupt_writer().map(|conn| conn.id) != Some(conn_id)
        {
            return refuse(InterruptRefusal::NotWriter);
        }
        self.active_turn.interrupt(generation)
    }
}

/// Test-only pause before the fence, so a test can interleave a writer
/// handoff with an interrupt that has already been read off the socket.
#[cfg(test)]
#[derive(Default)]
pub(super) struct InterruptProbe {
    gate: Mutex<Option<Arc<InterruptGate>>>,
}

#[cfg(test)]
pub(super) struct InterruptGate {
    conn_id: u64,
    /// One permit when the gated interrupt has been read and is about to take the fence.
    pub(super) reached: Semaphore,
    /// The gated interrupt proceeds to the fence once a permit is added.
    pub(super) release: Semaphore,
}

#[cfg(test)]
impl InterruptProbe {
    pub(super) async fn arm(&self, conn_id: u64) -> Arc<InterruptGate> {
        let gate = Arc::new(InterruptGate {
            conn_id,
            reached: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        *self.gate.lock().await = Some(gate.clone());
        gate
    }

    async fn before_fence(&self, conn_id: u64) {
        let gate = self.gate.lock().await.clone();
        if let Some(gate) = gate.filter(|gate| gate.conn_id == conn_id) {
            gate.reached.add_permits(1);
            gate.release
                .acquire()
                .await
                .expect("probe semaphore stays open")
                .forget();
        }
    }
}

impl AttachServer {
    /// Reader-side answer to one [`ClientFrame::InterruptTurn`]. Called from the
    /// connection's reader loop, outside the operator FIFO and `turn_serial`.
    /// The reply goes to the connection's own queue, so even a connection the
    /// registry has already dropped hears its honest refusal.
    pub(super) async fn answer_interrupt(
        &self,
        conn_id: u64,
        reply: &mpsc::Sender<DaemonFrame>,
        generation: TurnGeneration,
    ) {
        #[cfg(test)]
        self.interrupt_probe.before_fence(conn_id).await;
        let outcome = {
            let mut registry = self.registry.lock().await;
            let current_writer = self.last_writer_conn_id.load(Ordering::SeqCst);
            registry.interrupt_terminal_turn(conn_id, current_writer, generation)
        };
        if reply
            .try_send(DaemonFrame::InterruptResult {
                generation,
                outcome,
            })
            .is_err()
        {
            tracing::debug!(conn = conn_id, "daemon: interrupt result not delivered");
        }
    }

    /// Build the runtime (first activity), spawn the approval gate once, drive
    /// the turn. The turn emits to the daemon bus; the forwarder fans out + folds.
    pub(super) async fn drive_user_turn(&self, text: String) {
        if text.trim() == "/clear" {
            let _turn_guard = self.turn_serial.lock().await;
            let _action_guard = self.peer_reply_serial.lock().await;
            if let Some(handler) = self.peer_delivery.read().await.clone() {
                handler.clear_all_taint().await;
            }
            if let Err(error) = clear_conversation_rows(
                &self.pending_drafts,
                &self.peer_row_serial,
                &self.conversation,
                &self.registry,
                self.core.storage.as_ref(),
            )
            .await
            {
                tracing::warn!(%error, "clearing conversation rows failed");
            } else {
                let conversation_id = self.conversation.lock().await.id.clone();
                self.team_proposals
                    .withdraw_conversation(&conversation_id)
                    .await;
                let _row_guard = self.peer_row_serial.lock().await;
                if let Some(runtime) = self.core.built_runtime() {
                    runtime.recipient_context.clear().await;
                }
                self.broadcast_recipient_context().await;
            }
            return;
        }
        let _ = self
            .drive_user_turn_inner(text, ChannelKind::Terminal, None, TurnOrigin::Interactive)
            .await;
    }

    pub(super) async fn drive_channel_turn(&self, req: ChannelTurnRequest) {
        let _ = self
            .drive_user_turn_inner(
                req.text,
                req.origin,
                Some(req.response_tx),
                TurnOrigin::Channel,
            )
            .await;
    }

    /// One ordinary daemon turn, serialized on `turn_serial`. The slot spans
    /// exactly the serialized turn: begun after the mutex is held, ended on
    /// every exit path, and cleared only if it still names this generation.
    pub(super) async fn drive_user_turn_inner(
        &self,
        text: String,
        origin: ChannelKind,
        response_tx: Option<tokio::sync::oneshot::Sender<String>>,
        turn_origin: TurnOrigin,
    ) -> Result<(), String> {
        let _turn_guard = self.turn_serial.lock().await;
        let cancel = CancellationToken::new();
        let terminal =
            origin == ChannelKind::Terminal && matches!(turn_origin, TurnOrigin::Interactive);
        let generation = self
            .registry
            .lock()
            .await
            .begin_turn(terminal.then_some(&cancel));
        let result = self
            .run_serialized_turn(text, origin, response_tx, turn_origin, cancel)
            .await;
        self.registry.lock().await.end_turn(generation);
        result
    }

    async fn run_serialized_turn(
        &self,
        text: String,
        origin: ChannelKind,
        response_tx: Option<tokio::sync::oneshot::Sender<String>>,
        turn_origin: TurnOrigin,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        *self.active_channel_origin.lock().await = origin;
        *self.pending_channel_response_tx.lock().await = response_tx;
        self.operator_turn_active.store(true, Ordering::Release);

        let rt = match self.core.ensure_runtime().await {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "daemon: building turn runtime failed");
                self.settle_turn_state().await;
                return Err(e.to_string());
            }
        };
        self.ensure_approval_gate(rt.approval.clone()).await;

        let turn_complete = self.turn_complete.notified();
        let mut handle = {
            let mut conv = self.conversation.lock().await;
            rt.drive_turn(
                text,
                origin,
                &mut conv,
                &self.domain_tx,
                turn_origin,
                cancel.clone(),
            )
            .await
        };

        // `run_turn`'s token reaches tool execution but not a provider stream
        // that never yields, so the driver that owns the join handle races the
        // same token and aborts, exactly as the inbound-peer driver does.
        // `biased`: when both are ready the interrupt wins over a turn that
        // merely raced to its own end.
        let joined = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                handle.abort();
                let joined = (&mut handle).await;
                if joined.as_ref().is_err_and(tokio::task::JoinError::is_cancelled) {
                    self.close_interrupted_turn().await;
                    // Like the normal path: wait for the forwarder to fold THIS
                    // turn's terminal before settling, so its `notify_waiters()`
                    // cannot land on the next queued turn's fold wait.
                    let folded =
                        tokio::time::timeout(std::time::Duration::from_secs(5), turn_complete).await;
                    if folded.is_err() {
                        tracing::warn!(
                            "daemon turn was interrupted but its terminal was not folded before timeout"
                        );
                    }
                    self.settle_turn_state().await;
                    return Ok(());
                }
                joined
            }
            joined = &mut handle => joined,
        };
        if let Err(e) = joined {
            tracing::warn!(error = ?e, "daemon turn task failed");
            self.settle_turn_state().await;
            return Err(e.to_string());
        }
        let folded = tokio::time::timeout(std::time::Duration::from_secs(5), turn_complete).await;
        if folded.is_err() {
            tracing::warn!(
                "daemon turn completed but assistant commit was not observed before timeout"
            );
        }
        self.settle_turn_state().await;
        Ok(())
    }

    /// The abort dropped `run_turn` before it could emit a terminal chunk.
    /// Enqueue the ordered `Cancelled` terminal event behind every chunk the
    /// turn already produced: attachments project it and the forwarder folds it
    /// (an uncommitted partial answer is discarded, committed rows stay saved).
    async fn close_interrupted_turn(&self) {
        let conversation_id = self.conversation.lock().await.id.clone();
        let terminal = AppEvent::ProviderChunk {
            conversation_id,
            chunk: StreamChunk::TurnComplete {
                stop_reason: StopReason::Cancelled,
            },
        };
        let _ = self.domain_tx.send(terminal); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 21-2 A4 — ordered cancel terminal via the daemon forwarder channel, as inbound_turn's abort path
    }

    /// The per-turn daemon state every exit path resets.
    async fn settle_turn_state(&self) {
        self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
            .await;
        self.operator_turn_active.store(false, Ordering::Release);
        *self.active_channel_origin.lock().await = ChannelKind::Terminal;
    }

    pub(super) async fn resolve_pending_channel_response(&self, fallback: &str) {
        if let Some(tx) = self.pending_channel_response_tx.lock().await.take() {
            let _ = tx.send(fallback.to_string());
        }
    }
}

#[cfg(test)]
mod tests;
