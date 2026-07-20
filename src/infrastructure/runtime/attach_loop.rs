//! Rich attach-client TUI (Story 12.2c) — the *consumer* half of the daemon
//! attach feature.
//!
//! [`run_attached`] is a **slim, socket-sourced** TUI that supersedes the
//! line-based [`run_attach`](crate::adapters::daemon::attach_client::run_attach)
//! (12.2b) as the `daemon attach` entry point. It reuses the local render stack
//! verbatim (`TuiState`, `Conversation`, `StreamingState`, `reduce()`,
//! `update_streaming_mirror()`, the `chat_pane`/`status_bar`/`input_box` widgets)
//! but swaps:
//!
//! - the event **source**: local `domain_rx` → socket `read_frame::<DaemonFrame>`
//!   ([`apply_client_event`] folds each [`ClientEvent`]/`RawEvent` through the
//!   SAME `reduce()` path the local loop uses — there is no reverse
//!   `RawEvent → AppEvent` map, so a small client apply fn is required);
//! - the turn **sink**: local `run_turn` → [`SocketTurnDriver`] (the second
//!   [`TurnDriver`] impl, Q3), whose `submit` writes a `ClientFrame::UserMessage`
//!   over the socket instead of spawning a turn.
//!
//! There is **no local agent core** — the daemon owns the turn; the client holds
//! no provider/tools (AC3). Memory recall rides a normal daemon turn; only memory
//! *mutations* (`/memory consolidate` → 12.2d, `/memory forget` → deferred) need a
//! daemon-side path, which this client does not build.
//!
//! ## Why committed turns are appended, NOT rebuilt (reconciliation)
//!
//! The local loop drains `committed_turn → conversation.turns` then calls
//! `rebuild_messages_mirror()`. That is correct *there* because every message is
//! turn-backed. The daemon, however, persists a **messages-only** transcript
//! (`server.rs::commit_assistant_turn` pushes `ChatMessage`s, never `turns`), so a
//! freshly-attached client's `conversation.messages` (the snapshot) has no backing
//! `turns`. `rebuild_messages_mirror` removes any Assistant message whose id is not
//! in the processed-turn set — it would **wipe the entire historical transcript**.
//! The attach client therefore appends each completed turn as a `ChatMessage`
//! directly ([`turn_to_chat_message`]) and never rebuilds (verified against
//! `conversation.rs:154-286`).

#![cfg(unix)]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::adapters::daemon::protocol::{
    AttachMode, AttachSnapshot, ClientFrame, ConnectionTier, DaemonFrame, FrameError,
    ProtocolError, answer_attach_challenge, read_frame, write_frame,
};
use crate::adapters::rap::{AgentSigner, IdentityKeyStore};
use crate::adapters::tui::state::{AttachInfo, TabRenderState, TuiState};
use crate::adapters::tui::terminal;
use crate::adapters::tui::widgets::{chat_pane, input_box, status_bar};
use crate::domain::clock::{Clock, SystemClock};
use crate::domain::events::ChunkAction;
use crate::domain::models::session::SessionState;
use crate::domain::models::session_meta::now_unix;
use crate::domain::models::turn::{InvocationStatus, Turn, TurnPart, tool_call_id_for};
use crate::domain::models::visual::DensityMode;
use crate::domain::models::{
    ChannelKind, ChatMessage, Conversation, FocusState, MessageRole, PermissionMode,
    SessionManager, StatusState, StreamingState, ToolCall, ToolCallInfo, ToolResultInfo,
    generate_message_id,
};
use crate::domain::services::reducer::{ReducerState, reduce, update_streaming_mirror};
use crate::infrastructure::runtime::event_bus::{RawEvent, RawEventKind};
use crate::infrastructure::runtime::turn_driver::{TurnDriver, TurnViewState, UserSubmission};

/// The second [`TurnDriver`] implementation (Story 12.2c, Q3 — shared origination
/// seam). Where [`LocalTurnDriver`](crate::infrastructure::runtime::turn_driver::LocalTurnDriver)
/// assembles messages and spawns `run_turn`, `SocketTurnDriver::submit` forwards
/// the submission over the socket as a `ClientFrame::UserMessage`. Both loops
/// therefore originate turns through the identical `driver.submit(...)` door.
pub struct SocketTurnDriver {
    /// Bounded to the writer task (drains → `write_frame`). An `mpsc` (not a held
    /// lock) keeps the `MAX_KNOWN_STD_SYNC_LOCKS=4` ratchet untouched.
    frame_tx: mpsc::UnboundedSender<ClientFrame>,
}

impl SocketTurnDriver {
    pub fn new(frame_tx: mpsc::UnboundedSender<ClientFrame>) -> Self {
        Self { frame_tx }
    }
}

#[async_trait::async_trait]
impl TurnDriver for SocketTurnDriver {
    /// Forward the turn over the socket. Per the 12.2a AC3 classification table:
    /// - **row 2** — optimistically append the user `ChatMessage` locally (the
    ///   daemon does NOT echo the user message back on the event stream, so this
    ///   local copy is authoritative for the session and is reconciled by the
    ///   snapshot on re-attach — verified: `server.rs` emits no user-message event);
    /// - **row 15** — leave `*active_turn = None`; the daemon spawns the turn, so
    ///   the client holds no `JoinHandle` (cancellation is a protocol message, not
    ///   a local abort).
    async fn submit(&self, sub: UserSubmission, view: TurnViewState<'_>) {
        let UserSubmission {
            text,
            images,
            synthetic,
            turn_cancel,
            ..
        } = sub;

        // Forward to the daemon before mutating local history. A closed writer
        // channel means the socket writer already died; do not show a phantom
        // optimistic message that the daemon can never receive.
        if self
            .frame_tx
            .send(ClientFrame::UserMessage {
                text: text.clone(),
                images: images.clone(),
            })
            .is_err()
        {
            let _ = turn_cancel;
            *view.active_turn = None;
            view.state.status = StatusState::Flash {
                message: "attach socket closed — message not sent".into(),
                remaining_ms: 1500,
            };
            view.state.needs_redraw = true;
            return;
        }

        // row 2 — optimistic local append (origin Terminal: an attached terminal
        // client is the `Terminal` channel). It happens only after the frame is
        // accepted by the writer task.
        view.conversation.messages.push(ChatMessage {
            id: generate_message_id(),
            role: MessageRole::User,
            content: text,
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: now_unix(),
            token_count: None,
            stop_reason: None,
            synthetic,
            images: vec![],
            origin: ChannelKind::Terminal,
        });

        // row 15 — the daemon owns the turn; no local handle. Cancellation would be
        // a protocol message in a future story, so the token is intentionally unused.
        let _ = turn_cancel;
        *view.active_turn = None;

        view.state.status = StatusState::Streaming;
        view.state.needs_redraw = true;
    }
}

/// Perform the attach handshake over an already-connected, split socket: send
/// `Attach`, await `AttachAck`. Generic over `AsyncRead`/`AsyncWrite` so it is
/// driven deterministically over `UnixStream::pair()` in tests (no real socket,
/// no wall-clock flakiness — NFR49). It measures/owns ONLY the handshake; the
/// transcript rides inside `AttachAck.snapshot` today, so the NFR49 budget test
/// feeds a realistically large canned transcript (Murat).
pub async fn attach_handshake<R, W>(
    read_half: &mut R,
    write_half: &mut W,
    read_only_ok: bool,
    signer: &AgentSigner,
) -> Result<(AttachMode, AttachSnapshot)>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    // Server-first challenge handshake (Story 17.1a): answer the daemon's
    // one-use challenge with an Ed25519 proof of possession. The signer is
    // supplied by the caller (production loads it from the data directory; the
    // NFR49 budget test hands in a synthetic key — no disk or wall clock).
    answer_attach_challenge(
        read_half,
        write_half,
        read_only_ok,
        ConnectionTier::TrustedLocal,
        signer,
    )
    .await
    .context("sending attach handshake")?;

    match read_frame::<_, DaemonFrame>(read_half).await? {
        Some(DaemonFrame::AttachAck {
            granted_mode,
            snapshot,
        }) => Ok((granted_mode, snapshot)),
        Some(DaemonFrame::Error(e)) => Err(anyhow!("daemon rejected attach: {e}")),
        Some(other) => Err(anyhow!("unexpected first frame from daemon: {other:?}")),
        None => Err(anyhow!("daemon closed the connection during handshake")),
    }
}

/// Build a `ChatMessage` from a completed [`Turn`] (prose + reasoning text and a
/// projection of its tool invocations). Used in place of
/// `rebuild_messages_mirror` so the messages-only daemon history is never wiped
/// (see the module-level reconciliation note).
fn turn_to_chat_message(turn: &Turn) -> ChatMessage {
    let mut content = String::new();
    let mut outputs: HashMap<u64, (&str, bool)> = HashMap::new();
    for part in &turn.parts {
        if let TurnPart::ToolResult { refs, output, .. } = part {
            outputs.insert(refs.0, (output.content.as_str(), output.is_error));
        }
    }
    let mut tool_calls = Vec::new();
    for part in &turn.parts {
        match part {
            TurnPart::Prose { text, .. } | TurnPart::Reasoning { text, .. } => {
                content.push_str(text);
            }
            TurnPart::ToolInvocation {
                id,
                tool,
                args,
                status,
                started_at,
                ended_at,
            } => {
                let status_str = match status {
                    InvocationStatus::Success => Some("✓ Success"),
                    InvocationStatus::Error => Some("✗ Error"),
                    InvocationStatus::Cancelled => Some("⊘ Cancelled"),
                    InvocationStatus::Running | InvocationStatus::Pending => None,
                };
                let result = outputs.get(&id.0).map(|(c, is_err)| ToolResultInfo {
                    content: (*c).to_string(),
                    is_error: *is_err,
                });
                tool_calls.push(ToolCallInfo {
                    id: tool_call_id_for(&turn.id, *id),
                    name: tool.clone(),
                    input: args.clone(),
                    result,
                    started_at_ms: Some((*started_at).max(0) as u64),
                    completed_at_ms: ended_at.map(|v| v.max(0) as u64),
                    status: status_str.map(|s| s.to_string()),
                });
            }
            TurnPart::ToolResult { .. } => {}
        }
    }
    ChatMessage {
        id: turn.id.0.clone(),
        role: turn.role,
        content,
        content_blocks: vec![],
        tool_calls,
        created_at: turn.started_at / 1000,
        token_count: None,
        stop_reason: turn.stop_reason.clone(),
        synthetic: false,
        images: vec![],
        origin: ChannelKind::Terminal,
    }
}

/// A synthetic `System` message used for daemon-surfaced notices (purge notices,
/// read-only refusals, boundaries, blocked-action summaries). `synthetic` so the
/// `⤷` marker reads it as system-originated; `Terminal` origin so it carries no
/// channel prefix.
fn system_message(content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        id: generate_message_id(),
        role: MessageRole::System,
        content: content.into(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: now_unix(),
        token_count: None,
        stop_reason: None,
        synthetic: true,
        images: vec![],
        origin: ChannelKind::Terminal,
    }
}

fn tool_call_info_from_transition(call: &ToolCall) -> ToolCallInfo {
    let request = call.request();
    let result = match call {
        ToolCall::Success { result, .. } => Some(ToolResultInfo {
            content: result.output.clone(),
            is_error: result.is_error,
        }),
        ToolCall::Error { error, .. } => Some(ToolResultInfo {
            content: error.clone(),
            is_error: true,
        }),
        ToolCall::Cancelled { reason, .. } => Some(ToolResultInfo {
            content: reason.clone(),
            is_error: true,
        }),
        _ => None,
    };
    let started_at_ms = match call {
        ToolCall::Validating { started_at, .. } | ToolCall::Executing { started_at, .. } => {
            Some((*started_at).max(0) as u64)
        }
        _ => None,
    };

    ToolCallInfo {
        id: call.id().to_string(),
        name: request.tool_name.clone(),
        input: request.input.clone(),
        result,
        started_at_ms,
        completed_at_ms: None,
        status: Some(crate::domain::models::tool_call::status_chip(call).to_string()),
    }
}

/// The honest top-boundary marker (AC5 — no silent truncation). The full current
/// session rides in the snapshot, but earlier *session files* are not loaded
/// (NQ2 descope), so the top of the transcript says so rather than presenting a
/// blank edge as the true beginning.
fn session_boundary_message() -> ChatMessage {
    system_message("— session start — · Earlier sessions not loaded")
}

/// Seed the client's render conversation from the attach snapshot (AC5/AC8):
/// honest top boundary, then the full current-session transcript, then — if the
/// daemon denied any actions while unattended — a one-line waiting summary.
pub fn seed_conversation(snapshot: &AttachSnapshot) -> Conversation {
    let mut conv = Conversation {
        id: snapshot.conversation_id.clone(),
        ..Default::default()
    };
    conv.messages.push(session_boundary_message());
    conv.messages.extend(snapshot.transcript.iter().cloned());
    if snapshot.blocked_actions_waiting > 0 {
        conv.messages.push(system_message(format!(
            "⚠ {} action(s) waiting on you — denied while no client was attached. Re-run or approve now.",
            snapshot.blocked_actions_waiting
        )));
    }
    conv
}

/// Apply one socket-sourced [`ClientEvent`](crate::adapters::daemon::protocol::ClientEvent)
/// (`= RawEvent`) to the client's view state. This is the SAME `StreamChunk →
/// reduce()` path the local loop uses (`event_loop.rs:5364-5489`); only the source
/// is the socket. Returns whether a redraw is needed.
///
/// Unknown/future `RawEventKind` variants (it is `#[non_exhaustive]`) fall through
/// the `_` arm and are ignored — forward-compatibility for 12.3/12.4 (the split
/// insurance, Murat).
pub fn apply_client_event(
    raw: RawEvent,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    reducer: &mut ReducerState,
    status: &mut StatusState,
    permission_mode: &mut PermissionMode,
    clock: &dyn Clock,
) -> bool {
    match raw.kind {
        RawEventKind::Provider(chunk) => {
            let action = reduce(reducer, chunk, clock);
            update_streaming_mirror(reducer, streaming);
            if let Some(usage) = reducer.pending_usage.take() {
                conversation.usage = Some(usage);
            }
            if let Some(committed) = reducer.committed_turn.take() {
                // Append, do NOT rebuild (see module reconciliation note).
                conversation.messages.push(turn_to_chat_message(&committed));
            }
            if let ChunkAction::TurnComplete { .. } = action {
                *status = StatusState::Idle;
            } else {
                *status = StatusState::Streaming;
            }
            true
        }
        RawEventKind::ModeChanged(mode) => {
            *permission_mode = mode;
            true
        }
        RawEventKind::SystemNotice { message, .. } => {
            // The purge-notice (AC7) and other daemon notices arrive here; render
            // inline as a System message via the normal RawEvent path (no new frame).
            conversation.messages.push(system_message(message));
            true
        }
        RawEventKind::Tool(transition) => {
            let info = tool_call_info_from_transition(&transition.call);
            if let Some(tc) = streaming.active_tool_calls.get_mut(&info.id) {
                tc.status = info.status;
                if info.result.is_some() {
                    tc.result = info.result;
                }
                if info.started_at_ms.is_some() {
                    tc.started_at_ms = info.started_at_ms;
                }
            } else {
                streaming.active_tool_calls.insert(info.id.clone(), info);
            }
            true
        }
        // ModeChanged/SystemNotice/Tool handled above; everything else
        // (Approval/Mcp*/Capability/ConfigReloaded + any future variant) is not
        // acted on by the thin client — ignore gracefully, never panic.
        _ => false,
    }
}

/// Connect to this workspace's daemon and run the rich attach TUI until the user
/// detaches (`Esc`/`Ctrl+D`) or the daemon closes the connection. The daemon and
/// any in-flight turn keep running across detach (AC4).
pub async fn run_attached(workspace: &Path) -> Result<()> {
    let socket = crate::infrastructure::paths::daemon_socket_path(workspace)?;
    let stream = UnixStream::connect(&socket).await.with_context(|| {
        format!(
            "connecting to the daemon at {} — is it running? (`rustain daemon start`)",
            socket.display()
        )
    })?;
    let (mut read_half, mut write_half) = stream.into_split();

    // Server-first challenge handshake (writer half still owned here). Load (or
    // provision) this machine's identity key from the rustain data directory,
    // then answer the daemon's challenge with a proof of possession.
    let signer = IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?)
        .load_or_generate()
        .context("loading the local peer identity key")?;
    let (granted_mode, snapshot) =
        attach_handshake(&mut read_half, &mut write_half, false, &signer).await?;
    let read_only = granted_mode == AttachMode::ReadOnly;

    // Writer task: drains the frame channel → socket. Owns the write half for the
    // rest of the session; `Detach` and every `UserMessage` flow through it.
    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<ClientFrame>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if write_frame(&mut write_half, &frame).await.is_err() {
                break;
            }
        }
    });
    let driver = SocketTurnDriver::new(frame_tx.clone());

    // ── View state (reuses the local render stack) ──
    let mut conversation = seed_conversation(&snapshot);
    if read_only {
        // AC6 #2 — one-time, never-silent notice.
        conversation.messages.push(system_message(
            "👁 Read-only. Another client is attached with write access. Detach the other to take over.",
        ));
    }
    let mut streaming = StreamingState::default();
    let clock = SystemClock::default();
    let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
    let mut permission_mode = snapshot.permission_mode;
    let attach_info = AttachInfo {
        read_only,
        channel_count: snapshot.channels.len(),
    };
    let mut session_manager = SessionManager::new(SessionState::Empty);
    let mut active_turn: Option<tokio::task::JoinHandle<()>> = None;
    let mut input = String::new();
    let mut scroll_offset = 0usize;
    let mut auto_scroll = true;
    // Story 12.2d AC3 — consolidation card view-state (lives in loop locals, not AppState).
    let mut pending_consolidation_card: Option<
        crate::adapters::tui::state::PendingConsolidationCard,
    > = None;
    let mut pending_consolidation_token: Option<crate::adapters::daemon::protocol::ProposalToken> =
        None;

    // ── Terminal ──
    let mut term = terminal::setup(false)?;
    let size = term.size()?;
    let mut state = TuiState::new(size.width, size.height);
    let mut tab_render_state = TabRenderState::default();
    let tool_block_states: HashMap<
        String,
        crate::adapters::tui::widgets::tool_block::ToolBlockState,
    > = HashMap::new();
    let feedback_blocks: BTreeMap<String, crate::domain::models::FeedbackBlock> = BTreeMap::new();

    use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
    use futures::StreamExt;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let loop_result: Result<()> = loop {
        let _ = term.draw(|f| {
            let area = f.area();
            let Some(layout) = crate::adapters::tui::layout::compute_layout(
                area,
                &state.theme,
                &input,
                1,
                false,
                DensityMode::Focus,
            ) else {
                return;
            };
            chat_pane::render_attached(
                f,
                layout.chat_pane,
                &conversation,
                &streaming,
                scroll_offset,
                auto_scroll,
                &state.theme,
                &mut tab_render_state,
                &tool_block_states,
                &feedback_blocks,
            );
            // Story 12.2d AC3 — render the consolidation card with the IDENTICAL
            // bottom-anchored inline grammar the local TUI uses (bordered + accent,
            // event_loop.rs ~8925), not a hand-rolled borderless paragraph.
            if let Some(ref card) = pending_consolidation_card {
                use crate::adapters::tui::widgets::consolidation_card::render_consolidation_card_lines;
                use crate::adapters::tui::widgets::inline_card::render_bottom_anchored_card;
                let card_lines = render_consolidation_card_lines(card, &state.theme, layout.chat_pane.width);
                render_bottom_anchored_card(
                    f.buffer_mut(),
                    card_lines,
                    state.theme.colors.accent,
                    layout.chat_pane,
                );
            }
            status_bar::render(
                f,
                layout.status_bar,
                "(daemon)",
                None,
                &state.status,
                &state.theme,
                scroll_offset,
                &[],
                0,
                layout.chat_pane.height,
                permission_mode,
                conversation.usage.as_ref(),
                0,
                false,
                None,
                false,
                true,
                0,
                Some("Esc/Ctrl+D/Ctrl+C detach"),
                0,
                None,
                None,
                None,
                false,
                None,
                None,
                DensityMode::Focus,
                false,
                Some(&attach_info),
            );
            input_box::render(
                f,
                layout.input_area,
                &input,
                input.chars().count(),
                if read_only {
                    FocusState::Chat
                } else {
                    FocusState::Input
                },
                &state.theme,
                false,
                0,
                if read_only {
                    Some("read-only — can't send here")
                } else {
                    None
                },
            );
        });

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        match (key.code, key.modifiers) {
                            // Story 12.2d AC4/AC5 — consolidation card intercept.
                            (KeyCode::Char('y'), m) if !m.contains(KeyModifiers::CONTROL)
                                && pending_consolidation_token.is_some() =>
                            {
                                if let Some(token) = pending_consolidation_token.take() {
                                    let _ = frame_tx.send(ClientFrame::ConsolidationResolve {
                                        token,
                                        accept: true,
                                    });
                                }
                                pending_consolidation_card = None;
                                state.needs_redraw = true;
                            }
                            (KeyCode::Char('n'), m) | (KeyCode::Esc, m)
                                if !m.contains(KeyModifiers::CONTROL)
                                    && pending_consolidation_token.is_some() =>
                            {
                                // n or Esc while card shown → decline.
                                if let Some(token) = pending_consolidation_token.take() {
                                    let _ = frame_tx.send(ClientFrame::ConsolidationResolve {
                                        token,
                                        accept: false,
                                    });
                                }
                                pending_consolidation_card = None;
                                state.needs_redraw = true;
                            }
                            // AC4 — detach (keybinding only, no CLI verb).
                            (KeyCode::Esc, _)
                            | (KeyCode::Char('d'), KeyModifiers::CONTROL)
                            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                break Ok(());
                            }
                            (KeyCode::Enter, _) => {
                                if read_only {
                                    // AC6 #3 — inert, never silent.
                                    state.status = StatusState::Flash {
                                        message: "read-only — can't send here".into(),
                                        remaining_ms: 1500,
                                    };
                                } else if !input.trim().is_empty() {
                                    let sub = UserSubmission {
                                        text: std::mem::take(&mut input),
                                        images: vec![],
                                        synthetic: false,
                                        activation_set: None,
                                        agent_snapshot: None,
                                        turn_cancel: CancellationToken::new(),
                                    };
                                    let view = TurnViewState {
                                        conversation: &mut conversation,
                                        streaming: &mut streaming,
                                        state: &mut state,
                                        active_turn: &mut active_turn,
                                        session_manager: &mut session_manager,
                                    };
                                    driver.submit(sub, view).await;
                                    auto_scroll = true;
                                }
                            }
                            (KeyCode::Backspace, _) => {
                                if !read_only {
                                    input.pop();
                                }
                            }
                            (KeyCode::PageUp, _) => {
                                scroll_offset = scroll_offset.saturating_add(5);
                                auto_scroll = false;
                            }
                            (KeyCode::PageDown, _) => {
                                scroll_offset = scroll_offset.saturating_sub(5);
                                if scroll_offset == 0 {
                                    auto_scroll = true;
                                }
                            }
                            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                                if read_only {
                                    state.status = StatusState::Flash {
                                        message: "read-only — can't send here".into(),
                                        remaining_ms: 1500,
                                    };
                                } else {
                                    input.push(c);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Resize(w, h))) => {
                        state.terminal_width = w;
                        state.terminal_height = h;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(anyhow!("terminal input error: {e}")),
                    None => break Ok(()),
                }
            }
            frame = read_frame::<_, DaemonFrame>(&mut read_half) => {
                match frame {
                    Ok(Some(DaemonFrame::Event(raw))) => {
                        apply_client_event(
                            raw,
                            &mut conversation,
                            &mut streaming,
                            &mut reducer,
                            &mut state.status,
                            &mut permission_mode,
                            &clock,
                        );
                    }
                    Ok(Some(DaemonFrame::Error(ProtocolError::ReadOnly))) => {
                        // AC6 #1 — handle a daemon read-only refusal gracefully.
                        conversation.messages.push(system_message(
                            "⚠ read-only — the daemon refused your input (another client holds write).",
                        ));
                    }
                    Ok(Some(DaemonFrame::Error(e))) => {
                        conversation.messages.push(system_message(format!("[daemon error] {e}")));
                    }
                    Ok(Some(DaemonFrame::ApprovalRequest { tool, request_id, .. })) => {
                        // Rich approval card over attach is 12.2d; surface it honestly.
                        conversation.messages.push(system_message(format!(
                            "[approval needed] {tool} (id {}) — approve from the local TUI for now.",
                            request_id.0
                        )));
                    }
                    Ok(Some(DaemonFrame::ConsolidationProposed { token, proposals })) => {
                        // Story 12.2d AC3 — store token + build the card for rendering.
                        // Unwrap the per-item `ProposedFact` to the card's (MemoryFact, bool)
                        // shape (the id is the AI-12.2d-2 handle; 12.2d toggles all-on).
                        let card = crate::adapters::tui::state::PendingConsolidationCard {
                            conversation_id: String::new(),
                            proposals: proposals.into_iter().map(|pf| (pf.fact, true)).collect(),
                        };
                        pending_consolidation_card = Some(card);
                        pending_consolidation_token = Some(token);
                        state.needs_redraw = true;
                    }
                    Ok(Some(DaemonFrame::Detached)) => break Ok(()),
                    // AttachAck/History are not expected post-handshake; ignore.
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        conversation.messages.push(system_message("— daemon closed the connection —"));
                        break Ok(());
                    }
                    // Forward-compat: an undecodable / future-shaped frame body is
                    // logged and skipped — the stream survives (split insurance).
                    Err(FrameError::Json(e)) => {
                        tracing::warn!(error = %e, "attach: skipping undecodable frame (forward-compat)");
                    }
                    Err(e) => break Err(anyhow!("attach socket read error: {e}")),
                }
            }
            _ = tick.tick() => {}
        }
    };

    // Clean detach (AC4): tell the daemon, briefly observe `Detached`, restore.
    let _ = frame_tx.send(ClientFrame::Detach);
    let _ = tokio::time::timeout(
        Duration::from_millis(150),
        read_frame::<_, DaemonFrame>(&mut read_half),
    )
    .await;
    let _ = terminal::teardown(false);
    writer.abort();
    loop_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{NoticeLevel, StopReason, StreamChunk};
    use std::time::Instant;
    use tokio::net::UnixStream;

    /// Compile-time proof of the shared origination seam (Q3): `SocketTurnDriver`
    /// IS a `TurnDriver`, so `run_attached` originates turns through the same
    /// `driver.submit(...)` door the local loop's `submit_turn!` macro uses (AC3).
    const _: fn() = || {
        fn assert_impl<T: TurnDriver>() {}
        assert_impl::<SocketTurnDriver>();
    };

    fn user_msg(content: &str, origin: ChannelKind) -> ChatMessage {
        ChatMessage {
            id: generate_message_id(),
            role: MessageRole::User,
            content: content.into(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin,
        }
    }

    fn snapshot_with(transcript: Vec<ChatMessage>, blocked: usize) -> AttachSnapshot {
        AttachSnapshot {
            conversation_id: "conv-test".into(),
            transcript,
            permission_mode: PermissionMode::Normal,
            channels: vec![ChannelKind::Terminal],
            blocked_actions_waiting: blocked,
        }
    }

    fn provider(chunk: StreamChunk) -> RawEvent {
        RawEvent {
            conversation_id: Some("conv-test".into()),
            timestamp_ms: 0,
            kind: RawEventKind::Provider(chunk),
        }
    }

    /// Test 1 (NFR49) — the attach handshake completes within the 500ms budget
    /// against a REALISTICALLY LARGE canned transcript (decoupled from payload
    /// shape; Murat). Deterministic over `UnixStream::pair()`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_within_nfr49_budget_with_large_transcript() {
        let (client, server) = UnixStream::pair().unwrap();
        let (mut cr, mut cw) = client.into_split();
        let (mut sr, mut sw) = server.into_split();

        // ~500 messages of real-ish size — production-shaped, not empty.
        let big: Vec<ChatMessage> = (0..500)
            .map(|i| {
                let body = format!("message {i} with a paragraph of body text to give it weight. ")
                    .repeat(4);
                user_msg(&body, ChannelKind::Terminal)
            })
            .collect();

        let signer =
            AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]));
        let server_task = tokio::spawn(async move {
            // Server-first: issue a one-use challenge, then accept the proof.
            write_frame(
                &mut sw,
                &DaemonFrame::AttachChallenge {
                    nonce: b"nfr49-challenge".to_vec(),
                },
            )
            .await
            .unwrap();
            let _ = read_frame::<_, ClientFrame>(&mut sr).await.unwrap();
            write_frame(
                &mut sw,
                &DaemonFrame::AttachAck {
                    granted_mode: AttachMode::ReadWrite,
                    snapshot: snapshot_with(big, 0),
                },
            )
            .await
            .unwrap();
        });

        let start = Instant::now();
        let (mode, snap) = attach_handshake(&mut cr, &mut cw, false, &signer)
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(mode, AttachMode::ReadWrite);
        assert_eq!(snap.transcript.len(), 500);
        assert!(
            elapsed < Duration::from_millis(500),
            "handshake took {elapsed:?}, over the NFR49 500ms budget"
        );
        server_task.await.unwrap();
    }

    /// Test 2 — typed input is forwarded as a `UserMessage` frame + optimistically
    /// appended locally; no local turn handle is created (AC3 rows 2/15).
    #[tokio::test]
    async fn input_forwarded_as_user_message_frame() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientFrame>();
        let driver = SocketTurnDriver::new(tx);

        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut state = TuiState::new(80, 24);
        let mut active_turn: Option<tokio::task::JoinHandle<()>> = Some(tokio::spawn(async {}));
        let mut session_manager = SessionManager::new(SessionState::Empty);

        let sub = UserSubmission {
            text: "hi daemon".into(),
            images: vec![],
            synthetic: false,
            activation_set: None,
            agent_snapshot: None,
            turn_cancel: CancellationToken::new(),
        };
        let view = TurnViewState {
            conversation: &mut conversation,
            streaming: &mut streaming,
            state: &mut state,
            active_turn: &mut active_turn,
            session_manager: &mut session_manager,
        };
        driver.submit(sub, view).await;

        assert!(active_turn.is_none(), "no local turn handle (row 15)");
        assert_eq!(conversation.messages.len(), 1, "optimistic append (row 2)");
        assert_eq!(conversation.messages[0].content, "hi daemon");
        match rx.recv().await.unwrap() {
            ClientFrame::UserMessage { text, .. } => assert_eq!(text, "hi daemon"),
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn input_not_appended_when_writer_channel_closed() {
        let (tx, rx) = mpsc::unbounded_channel::<ClientFrame>();
        drop(rx);
        let driver = SocketTurnDriver::new(tx);

        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut state = TuiState::new(80, 24);
        let mut active_turn: Option<tokio::task::JoinHandle<()>> = Some(tokio::spawn(async {}));
        let mut session_manager = SessionManager::new(SessionState::Empty);

        let view = TurnViewState {
            conversation: &mut conversation,
            streaming: &mut streaming,
            state: &mut state,
            active_turn: &mut active_turn,
            session_manager: &mut session_manager,
        };
        driver
            .submit(
                UserSubmission {
                    text: "lost?".into(),
                    images: vec![],
                    synthetic: false,
                    activation_set: None,
                    agent_snapshot: None,
                    turn_cancel: CancellationToken::new(),
                },
                view,
            )
            .await;

        assert!(
            conversation.messages.is_empty(),
            "closed writer must not create a local phantom message"
        );
        assert!(active_turn.is_none(), "no local turn handle");
        assert!(
            matches!(state.status, StatusState::Flash { ref message, .. } if message.contains("not sent")),
            "user gets a visible send failure"
        );
    }

    /// Test 3 — a forwarded `Provider` chunk is applied through the SAME `reduce()`
    /// path as the local loop; on `TurnComplete` the assistant message lands in the
    /// transcript and status returns to Idle.
    #[test]
    fn render_from_client_event_via_reduce() {
        let clock = SystemClock::default();
        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut status = StatusState::Streaming;
        let mut mode = PermissionMode::Normal;

        apply_client_event(
            provider(StreamChunk::Text {
                content: "hello world".into(),
                parent_tool_use_id: None,
            }),
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut status,
            &mut mode,
            &clock,
        );
        assert_eq!(streaming.current_text_buffer, "hello world");
        assert!(streaming.is_streaming);

        apply_client_event(
            provider(StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }),
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut status,
            &mut mode,
            &clock,
        );
        assert_eq!(status, StatusState::Idle);
        let last = conversation
            .messages
            .last()
            .expect("assistant message appended");
        assert_eq!(last.role, MessageRole::Assistant);
        assert_eq!(last.content, "hello world");
    }

    #[test]
    fn tool_transition_inserts_missing_active_tool_call() {
        let clock = SystemClock::default();
        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut status = StatusState::Streaming;
        let mut mode = PermissionMode::Normal;

        apply_client_event(
            RawEvent {
                conversation_id: Some("conv-test".into()),
                timestamp_ms: 0,
                kind: RawEventKind::Tool(crate::domain::models::ToolCallTransition {
                    conversation_id: "conv-test".into(),
                    call: ToolCall::Executing {
                        id: "tool-1".into(),
                        request: crate::domain::models::ToolCallRequest {
                            id: "tool-1".into(),
                            tool_name: "Read".into(),
                            input: serde_json::json!({"file_path": "/tmp/x"}),
                        },
                        started_at: 42,
                    },
                }),
            },
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut status,
            &mut mode,
            &clock,
        );

        let tc = streaming
            .active_tool_calls
            .get("tool-1")
            .expect("late attach tool transition should create an active tool row");
        assert_eq!(tc.name, "Read");
        assert_eq!(tc.status.as_deref(), Some("● Executing"));
    }

    #[test]
    fn turn_to_chat_message_uses_canonical_tool_call_id() {
        let part_id = crate::domain::models::PartId(7);
        let mut turn = Turn::new("model".into(), 0);
        turn.id = crate::domain::models::TurnId("turn-a".into());
        turn.parts.push(TurnPart::ToolInvocation {
            id: part_id,
            tool: "Read".into(),
            args: serde_json::json!({"file_path": "/tmp/x"}),
            status: InvocationStatus::Success,
            started_at: 0,
            ended_at: Some(1),
        });

        let msg = turn_to_chat_message(&turn);
        assert_eq!(
            msg.tool_calls[0].id,
            tool_call_id_for(&turn.id, part_id),
            "attach loop must not drift from canonical chat-pane tool IDs"
        );
    }

    /// Test 4 (framing red test) — a `ChatMessage.content` with embedded newlines
    /// round-trips through `UnixStream::pair()` as ONE logical frame (the
    /// length-prefixed codec is newline-agnostic; a line-based reader would desync).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn newline_body_round_trips_as_single_frame() {
        let (client, server) = UnixStream::pair().unwrap();
        let (mut cr, _cw) = client.into_split();
        let (_sr, mut sw) = server.into_split();

        let body = "line one\nline two\nline three";
        let frame = DaemonFrame::AttachAck {
            granted_mode: AttachMode::ReadWrite,
            snapshot: snapshot_with(vec![user_msg(body, ChannelKind::Terminal)], 0),
        };
        let sender = tokio::spawn(async move {
            write_frame(&mut sw, &frame).await.unwrap();
            drop(sw); // clean EOF after exactly one frame
        });

        let first = read_frame::<_, DaemonFrame>(&mut cr).await.unwrap();
        match first {
            Some(DaemonFrame::AttachAck { snapshot, .. }) => {
                assert_eq!(snapshot.transcript[0].content, body);
                assert!(snapshot.transcript[0].content.contains('\n'));
            }
            other => panic!("expected one AttachAck frame, got {other:?}"),
        }
        // Exactly one frame: the next read is a clean EOF, not a desynced body.
        let second = read_frame::<_, DaemonFrame>(&mut cr).await.unwrap();
        assert!(second.is_none(), "newline body must not desync the reader");
        sender.await.unwrap();
    }

    /// Test 5 (forward-compat / split insurance) — a `RawEventKind` the thin client
    /// does not act on is ignored gracefully (no panic, view unchanged) and the
    /// stream survives: a subsequent `Provider` event still applies.
    #[test]
    fn unknown_event_kind_ignored_and_stream_survives() {
        let clock = SystemClock::default();
        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut status = StatusState::Idle;
        let mut mode = PermissionMode::Normal;

        let before = conversation.messages.len();
        let redraw = apply_client_event(
            RawEvent {
                conversation_id: None,
                timestamp_ms: 0,
                kind: RawEventKind::ConfigReloaded {
                    success: true,
                    error: None,
                },
            },
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut status,
            &mut mode,
            &clock,
        );
        assert!(!redraw, "unhandled kind requests no redraw");
        assert_eq!(conversation.messages.len(), before, "view unchanged");

        // Stream survives.
        apply_client_event(
            provider(StreamChunk::Text {
                content: "after".into(),
                parent_tool_use_id: None,
            }),
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut status,
            &mut mode,
            &clock,
        );
        apply_client_event(
            provider(StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }),
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut status,
            &mut mode,
            &clock,
        );
        assert_eq!(conversation.messages.last().unwrap().content, "after");
    }

    /// Test 6a (read-only true-by-absence) — the client→daemon protocol has NO
    /// memory-mutation frame, so an attach client *cannot* emit one. Enumerates the
    /// full `ClientFrame` set; if a memory-write frame is ever added this fails,
    /// forcing the read-only/ownership story to be revisited (the daemon-side e2e
    /// refusal lives in `server.rs` tests).
    #[test]
    fn client_protocol_has_no_memory_write_frame() {
        // Exhaustive match — adding a variant breaks compilation here on purpose.
        let sample = ClientFrame::Detach;
        match sample {
            ClientFrame::Attach { .. }
            | ClientFrame::UserMessage { .. }
            | ClientFrame::HistoryRequest { .. }
            | ClientFrame::ApprovalResponse { .. }
            | ClientFrame::InputResponse { .. }
            | ClientFrame::ConsolidationResolve { .. }
            | ClientFrame::PeerEnvelope(_)
            | ClientFrame::Detach => {}
        }
    }

    /// Test 7 (re-attach replay) — `Terminal` and `Telegram` origins in the
    /// snapshot seed into the attach render conversation and render their dimmed
    /// channel prefixes (provenance survives re-attach; AC2).
    #[test]
    fn attach_render_shows_terminal_and_telegram_channel_prefixes_after_seed() {
        use crate::adapters::tui::state::TabRenderState;
        use crate::adapters::tui::theme::Theme;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let snap = snapshot_with(
            vec![
                user_msg("hi from terminal", ChannelKind::Terminal),
                user_msg("hi from telegram", ChannelKind::Telegram),
            ],
            0,
        );
        let conv = seed_conversation(&snap);
        assert!(
            conv.messages
                .iter()
                .any(|m| m.origin == ChannelKind::Telegram),
            "telegram-origin message preserved through seed"
        );

        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Theme::dark();
        let streaming = StreamingState::default();
        let mut trs = TabRenderState::default();
        let tbs = HashMap::new();
        let fbs = BTreeMap::new();
        term.draw(|f| {
            chat_pane::render_attached(
                f,
                f.area(),
                &conv,
                &streaming,
                0,
                true,
                &theme,
                &mut trs,
                &tbs,
                &fbs,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut text = String::new();
        for y in 0..20 {
            for x in 0..80 {
                if let Some(c) = buf.cell((x, y)) {
                    text.push_str(c.symbol());
                }
            }
            text.push('\n');
        }
        assert!(
            text.contains("[terminal]"),
            "terminal channel prefix missing in attach render:\n{text}"
        );
        assert!(
            text.contains("[telegram]"),
            "telegram channel prefix missing in attach render:\n{text}"
        );
    }

    /// Test 9 — history is bottom-anchored (`auto_scroll`) with an honest top
    /// boundary (no silent truncation; AC5). The seed places the boundary first.
    #[test]
    fn seed_has_honest_top_boundary() {
        let snap = snapshot_with(vec![user_msg("a", ChannelKind::Terminal)], 0);
        let conv = seed_conversation(&snap);
        assert_eq!(conv.messages[0].role, MessageRole::System);
        assert!(conv.messages[0].content.contains("session start"));
        assert!(
            conv.messages[0]
                .content
                .contains("Earlier sessions not loaded")
        );
        assert_eq!(
            conv.messages[1].content, "a",
            "transcript follows the boundary"
        );
    }

    /// Test 11 — `blocked_actions_waiting > 0` surfaces a one-line waiting summary
    /// on attach so the user doesn't have to scroll to discover them (AC8).
    #[test]
    fn blocked_actions_waiting_surfaces_summary() {
        let snap = snapshot_with(vec![user_msg("a", ChannelKind::Terminal)], 3);
        let conv = seed_conversation(&snap);
        assert!(
            conv.messages
                .iter()
                .any(|m| m.content.contains("3 action(s) waiting on you")),
            "blocked-action summary missing"
        );

        // None waiting → no summary.
        let snap0 = snapshot_with(vec![user_msg("a", ChannelKind::Terminal)], 0);
        let conv0 = seed_conversation(&snap0);
        assert!(
            !conv0
                .messages
                .iter()
                .any(|m| m.content.contains("waiting on you")),
            "summary must not show when nothing is waiting"
        );
    }

    /// AC7 client half — a daemon-emitted purge-notice `SystemNotice` renders inline
    /// through the normal `RawEvent` path (no new frame).
    #[test]
    fn purge_notice_system_notice_renders_inline() {
        let clock = SystemClock::default();
        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut status = StatusState::Idle;
        let mut mode = PermissionMode::Normal;

        apply_client_event(
            RawEvent {
                conversation_id: None,
                timestamp_ms: 0,
                kind: RawEventKind::SystemNotice {
                    level: NoticeLevel::Info,
                    message: "5 facts removed from MEMORY.md — purged from search index".into(),
                },
            },
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut status,
            &mut mode,
            &clock,
        );
        assert!(
            conversation
                .messages
                .iter()
                .any(|m| m.content.contains("5 facts removed from MEMORY.md")),
            "purge-notice SystemNotice should render inline"
        );
    }
}
