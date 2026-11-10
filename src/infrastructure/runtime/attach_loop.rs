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

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
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
use crate::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource, NotificationStore};
use crate::adapters::tui::state::{AttachInfo, TabRenderState, TuiState};
use crate::adapters::tui::terminal;
use crate::adapters::tui::thread_inspector::pinned_response;
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

// The attached key front door lives in its own file but is a child of this
// module so it can use the private team/approval/surface machinery.
#[path = "attach_keys.rs"]
pub(super) mod attach_keys;

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
    /// - **row 15** — the daemon owns the turn; cancellation is a protocol
    ///   message, not a local abort.
    async fn submit(&self, sub: UserSubmission, view: TurnViewState<'_>) {
        // A caller changing from a local driver cannot strand its old producer.
        view.lifecycle.retire(&view.conversation.id);
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
            notifications::error(
                view.state,
                NoticeSource::Attached,
                "attach socket closed — message not sent",
            );
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
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        });

        // row 15 — the daemon owns the turn; no local handle. Cancellation would be
        // a protocol message in a future story, so the token is intentionally unused.
        let _ = turn_cancel;

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
        }) => {
            if granted_mode == AttachMode::ReadWrite {
                write_frame(write_half, &ClientFrame::EnableTeamProposals)
                    .await
                    .context("enabling attached team proposals")?;
            }
            Ok((granted_mode, snapshot))
        }
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
    // Carry the display-diff state through, exactly as the two sibling
    // reconstruction paths do (`Conversation::…` and the chat-pane shim).
    //
    // Story 19.1 code review, HIGH: this path used to keep only
    // `(content, is_error)` and hardcode the diff away, so an attached client
    // expanding a completed overwrite fell through to the input-derived
    // all-additions branch — painting a new-file diff over a replaced file,
    // which is precisely what ruling A4 forbids.
    let mut outputs: HashMap<u64, (&str, bool, &crate::domain::models::WriteDiffState)> =
        HashMap::new();
    for part in &turn.parts {
        if let TurnPart::ToolResult { refs, output, .. } = part {
            outputs.insert(
                refs.0,
                (output.content.as_str(), output.is_error, &output.diff),
            );
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
                let result = outputs.get(&id.0).map(|(c, is_err, diff)| ToolResultInfo {
                    content: (*c).to_string(),
                    is_error: *is_err,
                    diff: (*diff).clone(),
                    images: Vec::new(),
                });
                tool_calls.push(ToolCallInfo {
                    id: tool_call_id_for(&turn.id, *id),
                    name: tool.clone(),
                    input: args.clone(),
                    result,
                    started_at_ms: Some((*started_at).max(0) as u64),
                    completed_at_ms: ended_at.map(|v| v.max(0) as u64),
                    status: status_str.map(|s| s.to_string()),
                    provider_id: None,
                });
            }
            TurnPart::ToolResult { .. } => {}
            // Never forwarded to attach clients; never part of their view.
            TurnPart::ProviderReplay { .. } => {}
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
        history_source: turn.history_source,
        authorship: crate::domain::models::Authorship::HumanWritten,
        send_authorization: crate::domain::models::SendAuthorization::Unknown,
        retracted_at_ms: None,
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
        authorship: crate::domain::models::Authorship::HumanWritten,
        send_authorization: crate::domain::models::SendAuthorization::Unknown,
        retracted_at_ms: None,
        history_source: Default::default(),
    }
}

/// Unretracted automatic responses supported by the daemon, newest first.
/// Diagnostic/tool-evidence rows have no response identity, and pending response
/// controls remain ineligible even when an edited draft is agent-composed.
pub(crate) fn retractable_rows(conversation: &Conversation) -> impl Iterator<Item = &ChatMessage> {
    conversation.messages.iter().rev().filter(move |message| {
        message.send_authorization == crate::domain::models::SendAuthorization::Automatic
            && message.retracted_at_ms.is_none()
            && message
                .id
                .strip_prefix("peer-response-")
                .is_some_and(|node| !node.is_empty())
            && !conversation
                .peer_row_controls
                .get(&message.id)
                .is_some_and(|control| {
                    control.stage != crate::domain::models::PeerRowStage::Settled
                })
    })
}

/// A one-line preview of `content` bounded to `max_cells` terminal cells:
/// newlines become spaces, whole graphemes only, and a cut ends in `…` that is
/// counted inside the budget.
fn preview_line(content: &str, max_cells: usize) -> String {
    use crate::adapters::tui::text_geometry as geom;
    use unicode_segmentation::UnicodeSegmentation;

    let mut output = String::with_capacity(max_cells.saturating_mul(4));
    let mut used = 0usize;
    // Longest prefix that still leaves one cell for the marker.
    let mut marker_cut = 0usize;
    for grapheme in content.graphemes(true) {
        let grapheme = if grapheme.contains('\n') {
            " "
        } else {
            grapheme
        };
        let width = geom::cells(grapheme);
        if used.saturating_add(width) > max_cells {
            if max_cells == 0 {
                return output;
            }
            output.truncate(marker_cut);
            output.push('…');
            return output;
        }
        output.push_str(grapheme);
        used += width;
        if used < max_cells {
            marker_cut = output.len();
        }
    }
    output
}

async fn recipient_projection(
    workspace: &Path,
) -> Result<crate::adapters::policy::JournalRecipientItemProjection> {
    use crate::domain::ports::RoomJournalReader;
    let entries =
        crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
            workspace,
        )
        .load_entries()
        .await?;
    Ok(crate::adapters::policy::JournalRecipientItemProjection::from_entries(&entries))
}

/// `[v] View item` on a peer draft and `Tab` in the thread: the item the pinned node answers
/// (journal thread), or the node's response view (`want_node`, or a node without an item
/// address). Both are the one thread frame; neither offers a draft decision of its own.
async fn open_thread_for_node(
    workspace: &Path,
    conversation: &Conversation,
    node: &str,
    state: &mut TuiState,
    read_only: bool,
    want_node: bool,
) -> Result<()> {
    use crate::adapters::tui::state::ThreadAccess;

    let access = if read_only {
        ThreadAccess::ReadOnly
    } else {
        ThreadAccess::Writer
    };
    let (_, control) = pinned_response(conversation, node)
        .ok_or_else(|| anyhow!("response already settled — no action applied"))?;
    let Some(address) = control.item_address.as_ref().filter(|_| !want_node) else {
        return super::thread_open::open_node(conversation, node, state, access);
    };
    #[cfg(feature = "a2a")]
    {
        let projection = recipient_projection(workspace).await?;
        let item = crate::adapters::a2a::exec::recipient_item_for_node(
            &projection,
            &crate::domain::models::AgentId::from_peer_path(node)?,
        )
        .filter(|item| &item.address == address);
        if item.is_none() {
            return Err(anyhow!(
                "recipient item '{}' not found",
                address.item().as_str()
            ));
        }
    }
    super::thread_open::open_received(
        workspace,
        Some(conversation),
        address.item().as_str(),
        state,
        access,
        super::recipient_view::ReceivedThreadWindow::Newest,
    )
    .await
}

fn thread_flash(state: &mut TuiState, message: String) {
    crate::adapters::tui::handlers::recipient_actions::show_notice(state, None, message);
}

fn queue_recipient_action(
    state: &mut TuiState,
    frame: ClientFrame,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
    composed: bool,
) -> bool {
    use crate::adapters::tui::handlers::recipient_actions::{abort_unsent_action, begin_action};
    let correlated = matches!(
        frame,
        ClientFrame::ReplyToRecipientItem { .. }
            | ClientFrame::DeferRecipientItem { .. }
            | ClientFrame::AcknowledgeRecipientItem { .. }
            | ClientFrame::ShareRecipientStatus { .. }
            | ClientFrame::ShareRecipientRetro { .. }
            | ClientFrame::ResolvePeerDraft { .. }
            | ClientFrame::SetRecipientContext { .. }
    );
    if correlated {
        if !begin_action(state, &frame, composed) {
            return false;
        }
    } else if composed {
        state.input_buffer.clear();
        state.cursor_position = 0;
        state.input_scroll_offset = 0;
    }
    if let Err(error) = frame_tx.send(frame) {
        let message = error.to_string();
        abort_unsent_action(state, error.0, message);
        return false;
    }
    true
}

fn tool_call_info_from_transition(call: &ToolCall) -> ToolCallInfo {
    let request = call.request();
    let result = match call {
        ToolCall::Success { result, .. } => Some(ToolResultInfo {
            content: result.output.clone(),
            is_error: result.is_error,
            diff: crate::domain::models::WriteDiffState::NotAWrite,
            images: Vec::new(),
        }),
        ToolCall::Error { error, .. } => Some(ToolResultInfo {
            content: error.clone(),
            is_error: true,
            diff: crate::domain::models::WriteDiffState::NotAWrite,
            images: Vec::new(),
        }),
        ToolCall::Cancelled { reason, .. } => Some(ToolResultInfo {
            content: reason.clone(),
            is_error: true,
            diff: crate::domain::models::WriteDiffState::NotAWrite,
            images: Vec::new(),
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
        provider_id: None,
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
/// honest top boundary, then the full current-session transcript. The
/// blocked-actions summary is a store notice ([`blocked_actions_notice`]), never
/// a transcript row (Story 21.1 AC6).
pub fn seed_conversation(snapshot: &AttachSnapshot) -> Conversation {
    let mut conv = Conversation {
        id: snapshot.conversation_id.clone(),
        peer_row_revision: snapshot.peer_row_revision,
        ..Default::default()
    };
    conv.messages.push(session_boundary_message());
    for message in &snapshot.transcript {
        upsert_peer_row(
            &mut conv,
            message.clone(),
            snapshot.peer_row_controls.get(&message.id).cloned(),
        );
    }
    conv
}

/// The one-line waiting summary when the daemon denied actions while no client
/// was attached.
fn blocked_actions_notice(snapshot: &AttachSnapshot) -> Option<String> {
    (snapshot.blocked_actions_waiting > 0).then(|| {
        format!(
            "⚠ {} action(s) waiting on you — denied while no client was attached. Re-run or approve now.",
            snapshot.blocked_actions_waiting
        )
    })
}

fn seed_mirror_waiting(state: &mut TuiState, snapshot: &AttachSnapshot, read_only: bool) {
    if read_only {
        state
            .decisions
            .set_mirror_waiting(snapshot.retained_approvals + snapshot.pending_consent_cards);
    }
}

/// Story 21.1 AC6: the daemon frames that are notices (an oversized frame, a
/// daemon error or read-only refusal, the answer to a stop request) become
/// store notices with the same TTL, S-LOG and badge as the local host. None is
/// a transcript row and none changes the wire. The frame loop and the K9
/// keystone call this one helper.
fn apply_notice_frame(
    state: &mut TuiState,
    interrupt: &mut crate::infrastructure::runtime::attach_interrupt::AttachedInterrupt,
    frame: &DaemonFrame,
) {
    match frame {
        DaemonFrame::FrameTooLarge { frame, bytes } => notifications::warning(
            state,
            NoticeSource::Daemon,
            crate::adapters::daemon::protocol::frame_too_large_notice(frame, *bytes),
        ),
        // AC6 #1 — handle a daemon read-only refusal gracefully.
        DaemonFrame::Error(ProtocolError::ReadOnly) => notifications::warning(
            state,
            NoticeSource::Daemon,
            "⚠ read-only — the daemon refused your input (another client holds write).",
        ),
        DaemonFrame::Error(e) => {
            notifications::error(state, NoticeSource::Daemon, format!("[daemon error] {e}"))
        }
        DaemonFrame::ActiveTurn { .. } | DaemonFrame::InterruptResult { .. } => {
            if let Some(notice) = interrupt.apply(frame) {
                notifications::notify(
                    state,
                    NoticeSource::Attached,
                    NoticeSeverity::from_level(notice.level),
                    notice.text,
                );
            }
        }
        _ => {}
    }
    state.needs_redraw = true;
}

fn upsert_peer_row(
    conversation: &mut Conversation,
    message: ChatMessage,
    control: Option<crate::domain::models::PeerRowControl>,
) {
    let id = &message.id;
    if let Some(control) = control {
        conversation.peer_row_controls.insert(id.clone(), control);
    } else {
        conversation.peer_row_controls.remove(id);
    }
    if let Some(row) = conversation.messages.iter_mut().find(|row| row.id == *id) {
        *row = message;
    } else {
        conversation.messages.push(message);
    }
}

/// Apply the same row-id boundary to snapshot seed and ordered live operations.
pub fn apply_peer_row_operation(
    conversation: &mut Conversation,
    operation: crate::domain::models::PeerRowOperation,
) -> bool {
    use crate::domain::models::PeerRowOperation;
    if operation.revision() <= conversation.peer_row_revision {
        return false;
    }
    match operation {
        PeerRowOperation::Upsert {
            revision,
            message,
            control,
        } => {
            upsert_peer_row(conversation, message, control);
            conversation.peer_row_revision = revision;
        }
        PeerRowOperation::Remove { revision, row_id } => {
            conversation.messages.retain(|row| row.id != row_id);
            conversation.peer_row_controls.remove(&row_id);
            conversation.peer_row_revision = revision;
        }
    }
    true
}

/// Live rows use provider ids; committed rows use the reducer's turn/part id.
/// Retain that binding across commit so late tool transitions use the same notice.
#[derive(Default)]
struct AttachedToolInputs {
    reasons: HashMap<String, crate::adapters::daemon::protocol::ToolInputWithheld>,
    render_ids: HashMap<String, String>,
}

impl AttachedToolInputs {
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        event: RawEvent,
        withheld: Option<crate::adapters::daemon::protocol::ToolInputWithheld>,
        conversation: &mut Conversation,
        streaming: &mut StreamingState,
        reducer: &mut ReducerState,
        state: &mut TuiState,
        permission_mode: &mut PermissionMode,
        clock: &dyn Clock,
    ) {
        let (wire_id, invocation) = match &event.kind {
            RawEventKind::Provider(crate::domain::models::StreamChunk::ToolUse { id, .. })
            | RawEventKind::PeerProvider(crate::domain::models::StreamChunk::ToolUse {
                id, ..
            }) => (Some(id.clone()), true),
            RawEventKind::Tool(transition) => (Some(transition.call.id().to_owned()), false),
            _ => (None, false),
        };
        apply_client_event(
            event,
            conversation,
            streaming,
            reducer,
            state,
            permission_mode,
            clock,
        );
        let Some(wire_id) = wire_id else { return };
        if invocation {
            if let Some(turn) = &reducer.open_turn
                && let Some(part_id) = reducer.pending_invocations.get(&wire_id)
            {
                self.render_ids
                    .insert(wire_id.clone(), tool_call_id_for(&turn.id, *part_id));
            }
            // A new genuine null is not omitted, even if the provider recycles an id.
            self.reasons.remove(&wire_id);
        }
        if let Some(reason) = withheld {
            self.reasons.insert(wire_id.clone(), reason);
            if let Some(render_id) = self.render_ids.get(&wire_id) {
                self.reasons.insert(render_id.clone(), reason);
            }
        }
    }
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
    state: &mut TuiState,
    permission_mode: &mut PermissionMode,
    clock: &dyn Clock,
) -> bool {
    let peer_display = matches!(&raw.kind, RawEventKind::PeerProvider(_));
    match raw.kind {
        RawEventKind::Provider(chunk) | RawEventKind::PeerProvider(chunk) => {
            let action = reduce(reducer, chunk, clock);
            if peer_display {
                if let Some(turn) = reducer.open_turn.as_mut() {
                    turn.history_source = crate::domain::models::HistorySource::PeerDisplay;
                }
                if let Some(turn) = reducer.committed_turn.as_mut() {
                    turn.history_source = crate::domain::models::HistorySource::PeerDisplay;
                }
            }
            update_streaming_mirror(reducer, streaming);
            if let Some(usage) = reducer.pending_usage.take() {
                conversation.usage = Some(usage);
            }
            if let Some(committed) = reducer.committed_turn.take() {
                // Append, do NOT rebuild (see module reconciliation note).
                if !peer_display {
                    conversation.messages.push(turn_to_chat_message(&committed));
                }
            }
            if let ChunkAction::TurnComplete { .. } = action {
                state.status = StatusState::Idle;
            } else {
                state.status = StatusState::Streaming;
            }
            true
        }
        RawEventKind::PeerSystemNotice { message, .. } => {
            let mut row = system_message(message);
            row.history_source = crate::domain::models::HistorySource::PeerDisplay;
            conversation.messages.push(row);
            true
        }
        RawEventKind::ModeChanged(mode) => {
            *permission_mode = mode;
            true
        }
        RawEventKind::SystemNotice { level, message } => {
            // The purge-notice (AC7) and other daemon notices arrive here; they
            // become store notices (Story 21.1 AC6), never transcript rows.
            notifications::notify(
                state,
                NoticeSource::Daemon,
                NoticeSeverity::from_level(level),
                message,
            );
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
        RawEventKind::DomainEvent(payload) => {
            crate::adapters::tui::handlers::transparency::apply_domain_event(state, &payload)
        }
        // ModeChanged/SystemNotice/Tool/DomainEvent handled above; everything
        // else (Approval/Mcp*/Capability/ConfigReloaded + any future variant)
        // is not acted on by the thin client — ignore gracefully, never panic.
        _ => false,
    }
}

/// What an attached session does with one submitted input line that may be a
/// `/team` command.
///
/// Extracted so the intercept table is reachable by a test. `TeamCommandArgs`
/// is matched exhaustively: adding a parsed verb without an explicit attached
/// behavior fails compilation rather than becoming a model prompt.
#[derive(Debug)]
enum AttachedTeamLine {
    Frame(ClientFrame),
    Refused(String),
    PassThrough,
    LocalLog(crate::adapters::tui::handlers::team_command::TeamLogArgs),
    LocalView(String),
}

#[derive(Debug)]
enum AttachedTeamRequest {
    Send {
        block_id: String,
        action: Option<crate::domain::models::SendActionId>,
        /// The action's type, kept for rows a later start frame names.
        message_type: crate::domain::models::SemanticMessageType,
    },
    Board {
        next_sequence: u32,
        view: String,
    },
    Thread {
        next_sequence: u32,
        view: String,
        /// The read this request fills (`thread_open::open_sent`): its generation, and the
        /// request id as the origin the window was opened for.
        generation: u64,
        origin: String,
    },
    RetractPreview,
    Retract {
        next_sequence: u32,
        view: String,
        view_done: bool,
    },
    Status,
    Untrust,
}

#[derive(Default)]
pub(in crate::infrastructure::runtime) struct AttachedTeamRequests {
    next_id: u64,
    next_send_id: u64,
    pending: std::collections::BTreeMap<String, AttachedTeamRequest>,
    latest_board: Option<String>,
    latest_thread: Option<String>,
    open_retract: Option<String>,
    /// The attached client is an observer: its sent-thread windows are read-only.
    read_only: bool,
}

impl AttachedTeamRequests {
    fn mint(&mut self) -> String {
        self.next_id += 1;
        format!("attached-team-{}", self.next_id)
    }
}

/// Story 19.16g — what one composer key did on the attached client.
#[derive(Debug)]
enum ComposerOutcome {
    /// Edited, ignored, or refused aloud — nothing further to do.
    Handled,
    /// An intercepted `/team` frame was queued (never a model turn).
    FrameSent,
    /// Run a local `/team log` read now.
    LocalLog(crate::adapters::tui::handlers::team_command::TeamLogArgs),
    LocalView(String),
    /// Proceed to the draft-resolution / model-submission path.
    Submit(String),
}

/// Story 19.16g — composer routing for `Char`, `Backspace` and `Enter`,
/// extracted from `run_attached` (its production caller) so the read-only
/// exception is reachable by a test. Read-only clients may edit their composer,
/// run daemon-served team board/status/thread reads; model turns and
/// writer-only team commands remain guarded.
fn attached_composer_key(
    code: crossterm::event::KeyCode,
    read_only: bool,
    state: &mut TuiState,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
) -> ComposerOutcome {
    attached_composer_key_with_team_requests(
        code,
        read_only,
        state,
        frame_tx,
        &mut AttachedTeamRequests::default(),
    )
}

use crate::adapters::tui::keymap::table::ATTACHED_SIGN_IN;

fn attached_sign_in_command(input: &str) -> bool {
    matches!(
        crate::domain::services::slash_command::parse_slash_command(input),
        Some(("login" | "models", _))
    )
}

const ATTACHED_ACCOUNT_CHANGE: &str = "Account changes are not available in an attached session. Run `rustain auth accounts`, `rustain auth use <provider> --id <N>` or `rustain auth logout <provider> --id <N>` in a terminal.";

fn attached_account_command(input: &str) -> bool {
    matches!(
        crate::domain::services::slash_command::parse_slash_command(input),
        Some(("logout" | "accounts", _))
    )
}

fn attached_composer_key_with_team_requests(
    code: crossterm::event::KeyCode,
    read_only: bool,
    state: &mut TuiState,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
    team_requests: &mut AttachedTeamRequests,
) -> ComposerOutcome {
    use crossterm::event::KeyCode;
    let flash = |state: &mut TuiState, message: String| {
        notifications::info(state, NoticeSource::Attached, message);
    };
    if crate::adapters::tui::handlers::recipient_actions::pending_composition(state).is_some() {
        return ComposerOutcome::Handled;
    }
    match code {
        KeyCode::Char(c) => {
            let _ = crate::adapters::tui::app::type_char(state, c, None);
        }
        KeyCode::Backspace => {
            state.cursor_position = crate::adapters::tui::keymap::text_edit::delete_back(
                &mut state.input_buffer,
                state.cursor_position,
            );
        }
        KeyCode::Enter if state.input_buffer.trim().is_empty() => {
            if read_only {
                // AC6 #3 — inert, never silent.
                flash(state, "read-only — can't send here".into());
            }
        }
        // An attached client has no local provider layer or picker: answer
        // `/login` and `/models` here, never as a model turn.
        KeyCode::Enter if attached_sign_in_command(&state.input_buffer) => {
            state.input_buffer.clear();
            state.cursor_position = 0;
            state.input_scroll_offset = 0;
            notifications::info(state, NoticeSource::Login, ATTACHED_SIGN_IN);
        }
        // `/logout` and `/accounts` likewise; a CLI account change reaches the
        // daemon's built providers on their next request.
        KeyCode::Enter if attached_account_command(&state.input_buffer) => {
            state.input_buffer.clear();
            state.cursor_position = 0;
            state.input_scroll_offset = 0;
            notifications::info(state, NoticeSource::Accounts, ATTACHED_ACCOUNT_CHANGE);
        }
        KeyCode::Enter => {
            let request = team_requests.mint();
            match attached_team_line_for_request(&state.input_buffer, request) {
                AttachedTeamLine::LocalLog(args) => {
                    state.input_buffer.clear();
                    state.cursor_position = 0;
                    state.input_scroll_offset = 0;
                    return ComposerOutcome::LocalLog(args);
                }
                AttachedTeamLine::LocalView(item_id) => {
                    state.input_buffer.clear();
                    state.cursor_position = 0;
                    state.input_scroll_offset = 0;
                    return ComposerOutcome::LocalView(item_id);
                }
                AttachedTeamLine::Frame(ClientFrame::InspectRecipientContext) => {
                    if queue_recipient_action(
                        state,
                        ClientFrame::InspectRecipientContext,
                        frame_tx,
                        true,
                    ) {
                        return ComposerOutcome::FrameSent;
                    }
                }
                AttachedTeamLine::Frame(frame @ ClientFrame::TeamBoard { .. })
                | AttachedTeamLine::Frame(frame @ ClientFrame::TeamThread { .. })
                | AttachedTeamLine::Frame(frame @ ClientFrame::TeamStatus { .. }) => {
                    if enqueue_attached_team_frame(state, team_requests, frame, frame_tx) {
                        state.input_buffer.clear();
                        state.cursor_position = 0;
                        state.input_scroll_offset = 0;
                        return ComposerOutcome::FrameSent;
                    }
                }
                AttachedTeamLine::Frame(frame @ ClientFrame::TeamSend { .. })
                | AttachedTeamLine::Frame(frame @ ClientFrame::TeamFollowup { .. })
                | AttachedTeamLine::Frame(frame @ ClientFrame::TeamRetractPreview { .. })
                | AttachedTeamLine::Frame(frame @ ClientFrame::RevokeSenderConsent { .. }) => {
                    if read_only {
                        let message = if matches!(frame, ClientFrame::TeamRetractPreview { .. }) {
                            "read-only — can't retract here"
                        } else {
                            "read-only — can't send here"
                        };
                        flash(state, message.into());
                    } else {
                        if enqueue_attached_team_frame(state, team_requests, frame, frame_tx) {
                            state.input_buffer.clear();
                            state.cursor_position = 0;
                            state.input_scroll_offset = 0;
                            return ComposerOutcome::FrameSent;
                        }
                    }
                }
                AttachedTeamLine::Frame(frame) if read_only => {
                    flash(state, "read-only — can't send here".into());
                    let _ = frame;
                }
                AttachedTeamLine::Frame(frame) => {
                    if !queue_recipient_action(state, frame, frame_tx, true) {
                        return ComposerOutcome::Handled;
                    }
                    return ComposerOutcome::FrameSent;
                }
                AttachedTeamLine::Refused(error) => {
                    state.input_buffer.clear();
                    state.cursor_position = 0;
                    state.input_scroll_offset = 0;
                    flash(state, error);
                }
                AttachedTeamLine::PassThrough => {
                    if read_only {
                        flash(state, "read-only — can't send here".into());
                    } else {
                        state.cursor_position = 0;
                        state.input_scroll_offset = 0;
                        return ComposerOutcome::Submit(std::mem::take(&mut state.input_buffer));
                    }
                }
            }
        }
        _ => {}
    }
    ComposerOutcome::Handled
}

/// Story 19.16g — after the attached draw: only a frame that actually completed presents
/// the `/team log` window it painted. A failed draw, or a layout too small to paint, presents
/// nothing.
fn after_attached_draw(state: &mut TuiState, drawn: bool) {
    if drawn {
        state.log_visits_presented();
    } else {
        state.restore_unflushed_log_visits();
    }
}

fn begin_attached_team_send(
    state: &mut TuiState,
    recipients: &[String],
    message_type: crate::domain::models::SemanticMessageType,
) -> AttachedTeamRequest {
    use crate::adapters::tui::handlers::team_command::{
        TeamSendRow, render_team_send_summary, show_team_send,
    };
    state.team_send_next_id += 1;
    let block_id = format!("team-send-{}", state.team_send_next_id);
    let rows: Vec<_> = recipients
        .iter()
        .map(|alias| TeamSendRow {
            alias: alias.clone(),
            outcome: None,
            message_type,
        })
        .collect();
    show_team_send(state, &block_id, render_team_send_summary(&rows));
    state.team_send_blocks.insert(block_id.clone(), rows);
    AttachedTeamRequest::Send {
        block_id,
        action: None,
        message_type,
    }
}

/// Apply approval frames while keeping incomplete input outside the decision queue.
fn apply_attached_approval_frame(
    state: &mut TuiState,
    assemblies: &mut HashMap<
        crate::domain::models::tool_call::RequestId,
        crate::adapters::daemon::protocol::ExactInputAssembly,
    >,
    frame: DaemonFrame,
    read_only: bool,
) {
    use crate::adapters::daemon::protocol::{ApprovalRefusal, ExactInputAssembly};
    use crate::adapters::tui::decision_queue::{
        self, ConsentPayload, PermissionInput, PermissionPayload, Responder,
    };
    match frame {
        DaemonFrame::ApprovalRequest {
            request_id,
            tool,
            source,
            risk,
            summary,
            input_bytes,
        } if !read_only && !decision_queue::holds_request(state, &request_id) => {
            if tool == "a2a/sender-consent" {
                decision_queue::raise_consent(
                    state,
                    ConsentPayload {
                        request_id,
                        tool,
                        input_preview: summary,
                    },
                );
            } else if let Some(bytes) = input_bytes {
                assemblies.insert(request_id.clone(), ExactInputAssembly::new(bytes));
                decision_queue::raise_permission(
                    state,
                    PermissionPayload::new(
                        request_id,
                        source,
                        tool,
                        risk,
                        PermissionInput::Receiving { summary },
                    ),
                    Responder::Attached,
                );
            }
        }
        DaemonFrame::ApprovalInputChunk {
            request_id,
            sequence,
            data,
        } if !read_only => {
            if assemblies
                .get_mut(&request_id)
                .is_some_and(|assembly| assembly.push(sequence, &data).is_err())
            {
                assemblies.remove(&request_id);
            }
        }
        DaemonFrame::ApprovalInputComplete { request_id, chunks } if !read_only => {
            if let Some(assembly) = assemblies.remove(&request_id)
                && let Ok(value) = assembly.finish(chunks)
            {
                decision_queue::complete_input(state, &request_id, value);
            }
        }
        DaemonFrame::ApprovalClosed { request_id }
        | DaemonFrame::ApprovalRefused {
            request_id,
            reason: ApprovalRefusal::NoLongerPending,
        } => {
            assemblies.remove(&request_id);
            if read_only {
                state.decisions.retire_mirror(&request_id.0);
            } else {
                decision_queue::withdraw_request(state, &request_id);
            }
        }
        DaemonFrame::ApprovalRefused { reason, .. } => {
            notifications::warning(state, NoticeSource::Approval, reason.to_string());
        }
        _ => {}
    }
    state.needs_redraw = true;
}

fn attached_notice_badges(
    state: &TuiState,
) -> crate::adapters::tui::widgets::status_bar::NoticeBadges {
    crate::adapters::tui::widgets::status_bar::NoticeBadges {
        unseen: state.notifications.unseen(),
        decisions: crate::adapters::tui::decision_queue::waiting(state),
        mirror_waiting: state.decisions.mirror_waiting(),
        tail: attach_keys::status_tail(state),
    }
}

fn apply_attached_consolidation_frame(state: &mut TuiState, frame: DaemonFrame, read_only: bool) {
    if !read_only && let DaemonFrame::ConsolidationProposed { token, proposals } = frame {
        crate::adapters::tui::decision_queue::raise_attached_consolidation(
            state,
            token,
            proposals.into_iter().map(|pf| (pf.fact, true)).collect(),
        );
    }
}

/// Consume private DTOs explicitly; these never enter the public RawEvent bus.
fn apply_attached_proposal_frame(
    state: &mut TuiState,
    read_only: bool,
    requests: &mut AttachedTeamRequests,
    conversation_id: &str,
    frame: DaemonFrame,
) {
    use crate::adapters::daemon::protocol::TeamProposalCloseOutcome;
    match frame {
        DaemonFrame::TeamProposal {
            proposal,
            recipients,
            message_type,
            body,
        } => {
            if !read_only {
                crate::adapters::tui::decision_queue::raise_team_proposal(
                    state,
                    Arc::new(crate::domain::models::TeamProposal {
                        id: proposal,
                        conversation_id: conversation_id.to_owned(),
                        recipients,
                        message_type,
                        body,
                    }),
                    crate::adapters::tui::decision_queue::Responder::Attached,
                );
            }
        }
        DaemonFrame::TeamProposalClosed { proposal, outcome } => {
            let queued = state.decisions.items().any(|item| {
                item.kind == crate::adapters::tui::decision_queue::DecisionKind::TeamProposal
                    && item.conversation_id.as_deref() == Some(conversation_id)
                    && item.source == proposal
                    && item.responder == crate::adapters::tui::decision_queue::Responder::Attached
            });
            let sending = matches!(
                requests.pending.get(&proposal),
                Some(AttachedTeamRequest::Send { .. })
            );
            if !queued && !sending {
                return;
            }
            let busy = matches!(&outcome, TeamProposalCloseOutcome::Refused { reason }
                if reason == "Not sent yet — this daemon is busy sending. The proposal is still waiting.");
            if !busy {
                crate::adapters::tui::decision_queue::withdraw_team_proposal_id(
                    state,
                    conversation_id,
                    &proposal,
                    matches!(outcome, TeamProposalCloseOutcome::Withdrawn),
                );
            }
            if sending
                && let Some(AttachedTeamRequest::Send { block_id, .. }) =
                    requests.pending.remove(&proposal)
            {
                state.team_send_blocks.remove(&block_id);
                state.feedback_blocks.remove(&block_id);
            }
            let message = match outcome {
                TeamProposalCloseOutcome::Dismissed => {
                    "Proposal dismissed. Nothing was sent.".into()
                }
                TeamProposalCloseOutcome::Withdrawn => {
                    "Proposal withdrawn with the conversation. Nothing was sent.".into()
                }
                TeamProposalCloseOutcome::Refused { reason } => reason,
            };
            crate::adapters::tui::handlers::team_command::show_team_send(
                state,
                "team-proposal-status",
                message,
            );
        }
        _ => return,
    }
    state.needs_redraw = true;
}

fn enqueue_attached_team_frame(
    state: &mut TuiState,
    requests: &mut AttachedTeamRequests,
    frame: ClientFrame,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
) -> bool {
    let (request, pending) = match &frame {
        ClientFrame::TeamSend {
            request,
            recipients,
            message_type,
            ..
        } => (
            request.clone(),
            begin_attached_team_send(state, recipients, message_type.unwrap_or_default()),
        ),
        ClientFrame::TeamFollowup { request, peer, .. } => (
            request.clone(),
            begin_attached_team_send(
                state,
                std::slice::from_ref(peer),
                crate::domain::models::SemanticMessageType::Unknown,
            ),
        ),
        ClientFrame::TeamThread {
            request,
            peer,
            item_id,
        } => {
            requests
                .pending
                .retain(|_, pending| !matches!(pending, AttachedTeamRequest::Thread { .. }));
            requests.latest_thread = Some(request.clone());
            let access = if requests.read_only {
                crate::adapters::tui::state::ThreadAccess::ReadOnly
            } else {
                crate::adapters::tui::state::ThreadAccess::Writer
            };
            let generation = super::thread_open::open_sent(state, access, peer, item_id, request);
            (
                request.clone(),
                AttachedTeamRequest::Thread {
                    next_sequence: 0,
                    view: String::new(),
                    generation,
                    origin: request.clone(),
                },
            )
        }
        ClientFrame::TeamRetractPreview { request, .. } => {
            if requests.open_retract.is_some()
                || requests.pending.values().any(|pending| {
                    matches!(
                        pending,
                        AttachedTeamRequest::RetractPreview | AttachedTeamRequest::Retract { .. }
                    )
                })
            {
                crate::adapters::tui::handlers::team_command::show_team_retract(
                    state,
                    "A retract is already awaiting your answer. Nothing was sent for this one."
                        .into(),
                );
                return false;
            }
            (request.clone(), AttachedTeamRequest::RetractPreview)
        }
        ClientFrame::TeamBoard { request, .. } => {
            requests
                .pending
                .retain(|_, pending| !matches!(pending, AttachedTeamRequest::Board { .. }));
            requests.latest_board = Some(request.clone());
            (
                request.clone(),
                AttachedTeamRequest::Board {
                    next_sequence: 0,
                    view: String::new(),
                },
            )
        }
        ClientFrame::TeamStatus { request } => (request.clone(), AttachedTeamRequest::Status),
        ClientFrame::RevokeSenderConsent { request, .. } => {
            (request.clone(), AttachedTeamRequest::Untrust)
        }
        _ => return false,
    };
    requests.pending.insert(request.clone(), pending);
    if frame_tx.send(frame).is_ok() {
        true
    } else {
        if let Some(pending) = requests.pending.remove(&request) {
            paint_team_request_failure(state, pending, "could not enqueue command");
        }
        false
    }
}

fn paint_team_request_failure(state: &mut TuiState, pending: AttachedTeamRequest, message: &str) {
    use crate::adapters::tui::handlers::team_command::{
        show_team_board, show_team_send, show_team_status,
    };
    let text = format!("[daemon error] {message}");
    match pending {
        AttachedTeamRequest::Send { block_id, .. } => {
            state.team_send_blocks.remove(&block_id);
            show_team_send(state, &block_id, text);
        }
        AttachedTeamRequest::Board { .. } => show_team_board(state, text),
        AttachedTeamRequest::Thread {
            generation, origin, ..
        } => {
            super::thread_open::fill_sent(state, &origin, generation, &text);
        }
        AttachedTeamRequest::RetractPreview | AttachedTeamRequest::Retract { .. } => {
            crate::adapters::tui::handlers::team_command::show_team_retract(state, text)
        }
        AttachedTeamRequest::Status | AttachedTeamRequest::Untrust => show_team_status(state, text),
    }
}

fn apply_attached_team_frame(
    state: &mut TuiState,
    requests: &mut AttachedTeamRequests,
    frame: DaemonFrame,
    confirmation_busy: bool,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
) -> bool {
    use crate::adapters::tui::handlers::team_command::{
        render_team_send_summary, show_team_board, show_team_send, show_team_status,
    };
    match frame {
        DaemonFrame::TeamSendStarted {
            request,
            action,
            recipients,
        } => {
            let Some(AttachedTeamRequest::Send {
                block_id,
                action: bound,
                message_type,
            }) = requests.pending.get_mut(&request)
            else {
                tracing::warn!(%request, "attach: unmatched team send start");
                return true;
            };
            if bound.is_some() {
                tracing::warn!(%request, "attach: duplicate team send start");
                return true;
            }
            *bound = Some(action);
            if !recipients.is_empty() {
                let message_type = *message_type;
                let rows = recipients
                    .into_iter()
                    .map(
                        |alias| crate::adapters::tui::handlers::team_command::TeamSendRow {
                            alias,
                            outcome: None,
                            message_type,
                        },
                    )
                    .collect();
                state.team_send_blocks.insert(block_id.clone(), rows);
            }
            let message = state
                .team_send_blocks
                .get(block_id)
                .map(|rows| render_team_send_summary(rows));
            if let Some(message) = message {
                show_team_send(state, block_id, message);
            }
            true
        }
        DaemonFrame::TeamSendSettled {
            request,
            index,
            outcome,
        } => {
            let Some(AttachedTeamRequest::Send { block_id, .. }) = requests.pending.get(&request)
            else {
                tracing::warn!(%request, "attach: unmatched team send settlement");
                return true;
            };
            let block_id = block_id.clone();
            let not_sent = outcome == crate::adapters::daemon::protocol::TeamSendOutcome::NotSent;
            let settled = crate::infrastructure::runtime::transparency_bridge::settle_team_send_row(
                &mut state.team_send_blocks,
                &mut state.feedback_blocks,
                &block_id,
                index,
                outcome.into(),
            );
            let landed = settled.as_ref().is_some_and(|settled| settled.redraw);
            if let Some(settled) = settled {
                crate::infrastructure::runtime::transparency_bridge::post_send_notices(
                    state,
                    settled.notices,
                );
            }
            if !landed {
                // Story 19.23: a selected send has no rows until its start
                // frame names them. `NotSent` before any start means the daemon
                // could not record the action and sent nothing; say so and
                // retire the request rather than leave an empty block pending.
                if not_sent
                    && matches!(
                        requests.pending.get(&request),
                        Some(AttachedTeamRequest::Send { action: None, .. })
                    )
                    && state
                        .team_send_blocks
                        .get(&block_id)
                        .is_some_and(Vec::is_empty)
                {
                    requests.pending.remove(&request);
                    state.team_send_blocks.remove(&block_id);
                    show_team_send(
                        state,
                        &block_id,
                        format!(
                            "  {}",
                            crate::domain::models::team_send::TEAM_SEND_NOT_SENT_LABEL
                        ),
                    );
                    state.needs_redraw = true;
                    return true;
                }
                tracing::warn!(%request, index, "attach: mismatched team send settlement");
                return true;
            }
            state.needs_redraw = true;
            if !state.team_send_blocks.contains_key(&block_id) {
                requests.pending.remove(&request);
            }
            true
        }
        DaemonFrame::TeamViewChunk {
            request,
            sequence,
            view,
        } => {
            let current = match requests.pending.get(&request) {
                Some(AttachedTeamRequest::Board { .. }) => {
                    requests.latest_board.as_deref() == Some(request.as_str())
                }
                Some(AttachedTeamRequest::Thread { .. }) => {
                    requests.latest_thread.as_deref() == Some(request.as_str())
                }
                Some(AttachedTeamRequest::Retract { view_done, .. }) => !view_done,
                _ => {
                    tracing::warn!(%request, "attach: unmatched view chunk");
                    return true;
                }
            };
            let Some(pending) = requests.pending.get_mut(&request) else {
                return true;
            };
            let (next_sequence, assembled) = match pending {
                AttachedTeamRequest::Board {
                    next_sequence,
                    view,
                }
                | AttachedTeamRequest::Thread {
                    next_sequence,
                    view,
                    ..
                }
                | AttachedTeamRequest::Retract {
                    next_sequence,
                    view,
                    ..
                } => (next_sequence, view),
                _ => return true,
            };
            if !current || *next_sequence != sequence || sequence == u32::MAX {
                if let AttachedTeamRequest::Retract {
                    view_done, view, ..
                } = pending
                {
                    *view_done = true;
                    view.clear();
                } else {
                    requests.pending.remove(&request);
                }
                tracing::warn!(%request, "attach: stale or out-of-order view discarded");
                return true;
            }
            assembled.push_str(&view);
            *next_sequence += 1;
            true
        }
        DaemonFrame::TeamViewDone { request, chunks } => {
            let Some(pending) = requests.pending.remove(&request) else {
                tracing::warn!(%request, "attach: unmatched view completion");
                return true;
            };
            match pending {
                AttachedTeamRequest::Board {
                    next_sequence,
                    view,
                } if chunks == next_sequence
                    && requests.latest_board.as_deref() == Some(request.as_str()) =>
                {
                    show_team_board(state, view)
                }
                AttachedTeamRequest::Thread {
                    next_sequence,
                    view,
                    generation,
                    origin,
                } if chunks == next_sequence
                    && requests.latest_thread.as_deref() == Some(request.as_str()) =>
                {
                    super::thread_open::fill_sent(state, &origin, generation, &view);
                }
                AttachedTeamRequest::Retract {
                    next_sequence,
                    view,
                    view_done,
                } => {
                    if !view_done && chunks == next_sequence {
                        show_team_board(state, view);
                    }
                    requests.pending.insert(
                        request,
                        AttachedTeamRequest::Retract {
                            next_sequence,
                            view: String::new(),
                            view_done: true,
                        },
                    );
                }
                other @ (AttachedTeamRequest::Send { .. }
                | AttachedTeamRequest::Status
                | AttachedTeamRequest::Untrust
                | AttachedTeamRequest::RetractPreview) => {
                    requests.pending.insert(request, other);
                }
                _ => tracing::warn!(%request, "attach: incomplete or stale view discarded"),
            }
            true
        }
        DaemonFrame::TeamRetractPreviewed { request, preview } => {
            use crate::adapters::daemon::protocol::TeamRetractPreviewView;
            use crate::adapters::tui::handlers::team_command as handler;
            if !matches!(
                requests.pending.get(&request),
                Some(AttachedTeamRequest::RetractPreview)
            ) {
                tracing::warn!(%request, "attach: unmatched retract preview");
                return true;
            }
            match preview {
                TeamRetractPreviewView::Answer { message } => {
                    requests.pending.remove(&request);
                    requests.open_retract = None;
                    handler::show_team_retract(state, message);
                }
                TeamRetractPreviewView::Card {
                    peer,
                    item_id,
                    body,
                    armed,
                } => {
                    use crate::adapters::tui::decision_queue::{
                        self, DecisionKind, Responder, RetractTarget, TeamRetractPayload,
                    };
                    if confirmation_busy
                        || decision_queue::confirmation_pending(state)
                        || state.decisions.presenting(DecisionKind::Permission)
                        || state.decisions.presenting(DecisionKind::Consent)
                        || requests.open_retract.is_some()
                        || requests.pending.iter().any(|(id, pending)| {
                            id != &request
                                && matches!(
                                    pending,
                                    AttachedTeamRequest::RetractPreview
                                        | AttachedTeamRequest::Retract { .. }
                                )
                        })
                    {
                        requests.pending.remove(&request);
                        let _ = frame_tx.send(ClientFrame::TeamRetractDecision {
                            request,
                            confirm: false,
                        });
                        handler::show_team_retract(state,
                            "Another confirmation is already awaiting your answer. Nothing was sent for this one.".into());
                    } else {
                        use crate::domain::services::transparency::{
                            MAX_PEER_ID_BYTES, sanitize_disclosable,
                        };
                        let shown_peer = sanitize_disclosable(&peer, MAX_PEER_ID_BYTES);
                        let shown_item = sanitize_disclosable(&item_id, MAX_PEER_ID_BYTES);
                        let mut body = body
                            .replace(&format!("{peer}'s host"), &format!("{shown_peer}'s host"))
                            .replace(
                                &format!("addresses {peer} with"),
                                &format!("addresses {shown_peer} with"),
                            )
                            .replace(
                                &format!("item         {item_id}"),
                                &format!("item         {shown_item}"),
                            );
                        let (yes, no) = {
                            use crate::adapters::tui::keymap::{
                                Context, render::rail, table_card::CardCmd,
                            };
                            (
                                rail(Context::DecisionConfirm, CardCmd::Yes),
                                rail(Context::DecisionConfirm, CardCmd::No),
                            )
                        };
                        let has_keys = |text: &str| text.contains(&yes) || text.contains(&no);
                        if let Some((prefix, last)) = body.rsplit_once("\n\n") {
                            if has_keys(last) {
                                body = prefix.trim_end().to_owned();
                            }
                        } else if has_keys(&body) {
                            body.clear();
                        }
                        decision_queue::raise_team_retract(
                            state,
                            TeamRetractPayload {
                                target: RetractTarget::Item {
                                    peer,
                                    item: item_id,
                                },
                                conversation_id: String::new(),
                                principal: None,
                                task: None,
                                card: body,
                                armed,
                                request: Some(request.clone()),
                            },
                            Responder::Attached,
                        );
                        requests.open_retract = Some(request);
                    }
                }
            }
            true
        }
        DaemonFrame::TeamRetractAnswered { request, message } => {
            if matches!(
                requests.pending.get(&request),
                Some(AttachedTeamRequest::Retract { .. })
            ) {
                requests.pending.remove(&request);
                requests.open_retract = None;
                crate::adapters::tui::decision_queue::withdraw_team_retract_request(
                    state, &request,
                );
                crate::adapters::tui::handlers::team_command::show_team_retract(state, message);
            } else {
                tracing::warn!(%request, "attach: unmatched retract answer");
            }
            true
        }
        DaemonFrame::TeamStatusResult { request, message } => {
            if !matches!(
                requests.pending.get(&request),
                Some(AttachedTeamRequest::Status)
            ) {
                tracing::warn!(%request, "attach: mismatched team status result");
                return true;
            }
            requests.pending.remove(&request);
            show_team_status(state, message);
            true
        }
        DaemonFrame::SenderConsentRevoked {
            request, message, ..
        } => {
            if !matches!(
                requests.pending.get(&request),
                Some(AttachedTeamRequest::Untrust)
            ) {
                tracing::warn!(%request, "attach: mismatched sender-consent result");
                return true;
            }
            requests.pending.remove(&request);
            show_team_status(state, message);
            true
        }
        _ => false,
    }
}

fn apply_attached_team_refusal(
    state: &mut TuiState,
    requests: &mut AttachedTeamRequests,
    action: crate::adapters::daemon::protocol::RecipientAction,
    request: String,
    outcome: crate::adapters::daemon::protocol::RecipientActionOutcome,
) -> bool {
    use crate::adapters::daemon::protocol::{RecipientAction, RecipientActionOutcome};
    let expected = match requests.pending.get(&request) {
        Some(AttachedTeamRequest::Send { .. }) => RecipientAction::TeamSend,
        Some(AttachedTeamRequest::Board { .. }) => RecipientAction::TeamBoard,
        Some(AttachedTeamRequest::Thread { .. }) => RecipientAction::TeamThread,
        Some(AttachedTeamRequest::RetractPreview | AttachedTeamRequest::Retract { .. }) => {
            RecipientAction::TeamRetract
        }
        Some(AttachedTeamRequest::Status) => RecipientAction::TeamStatus,
        Some(AttachedTeamRequest::Untrust) => RecipientAction::RevokeSenderConsent,
        None => {
            tracing::warn!(%request, "attach: unmatched team refusal");
            return true;
        }
    };
    if action != expected {
        tracing::warn!(%request, ?action, "attach: mismatched team refusal kind");
        return true;
    }
    if !matches!(&outcome, RecipientActionOutcome::Refused { .. }) {
        tracing::warn!(%request, "attach: request result was not a refusal");
        return true;
    }
    let Some(pending) = requests.pending.remove(&request) else {
        return true;
    };
    let message = match outcome {
        RecipientActionOutcome::Refused { error } => format!("[daemon error] {error}"),
        other => {
            tracing::warn!(%request, ?other, "attach: unexpected non-refusal team action result");
            return true;
        }
    };
    match pending {
        AttachedTeamRequest::Send { block_id, .. } => {
            state.team_send_blocks.remove(&block_id);
            crate::adapters::tui::handlers::team_command::show_team_send(state, &block_id, message)
        }
        AttachedTeamRequest::Board { .. } => {
            if requests.latest_board.as_deref() == Some(request.as_str()) {
                crate::adapters::tui::handlers::team_command::show_team_board(state, message);
            }
        }
        AttachedTeamRequest::Thread {
            generation, origin, ..
        } => {
            if requests.latest_thread.as_deref() == Some(request.as_str()) {
                super::thread_open::fill_sent(state, &origin, generation, &message);
            }
        }
        AttachedTeamRequest::RetractPreview | AttachedTeamRequest::Retract { .. } => {
            if requests.open_retract.as_deref() == Some(request.as_str()) {
                requests.open_retract = None;
            }
            crate::adapters::tui::decision_queue::withdraw_team_retract_request(state, &request);
            crate::adapters::tui::handlers::team_command::show_team_retract(state, message);
        }
        AttachedTeamRequest::Status | AttachedTeamRequest::Untrust => {
            crate::adapters::tui::handlers::team_command::show_team_status(state, message)
        }
    }
    true
}
fn attached_team_line(text: &str) -> AttachedTeamLine {
    attached_team_line_for_request(text, "test-request".to_owned())
}

fn attached_team_line_for_request(text: &str, request: String) -> AttachedTeamLine {
    use crate::adapters::tui::handlers::team_command::{TeamCommandArgs, parse_team_command};

    if text.trim() == "/clear" {
        return AttachedTeamLine::Frame(ClientFrame::UserMessage {
            text: "/clear".to_owned(),
            images: Vec::new(),
        });
    }
    let line = text.trim_start();
    let Some(arg) = line.strip_prefix("/team") else {
        return AttachedTeamLine::PassThrough;
    };
    if !arg.is_empty() && !arg.starts_with(char::is_whitespace) {
        return AttachedTeamLine::PassThrough;
    }
    match parse_team_command(Some(arg.trim_start())) {
        Ok(TeamCommandArgs::Log(args)) => AttachedTeamLine::LocalLog(args),
        Ok(TeamCommandArgs::Board { selector }) => {
            AttachedTeamLine::Frame(ClientFrame::TeamBoard {
                request,
                selector: (&selector).into(),
            })
        }
        Ok(TeamCommandArgs::Send {
            recipients,
            text,
            message_type,
            subject,
            stories,
            holders_of,
        }) => AttachedTeamLine::Frame(ClientFrame::TeamSend {
            request,
            recipients,
            text,
            message_type,
            subject,
            stories,
            holders_of,
        }),
        Ok(TeamCommandArgs::Acknowledge { item_id }) => {
            AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { item_id })
        }
        Ok(TeamCommandArgs::Remove { item_id }) => {
            AttachedTeamLine::Frame(ClientFrame::RemoveRecipientItem { item_id })
        }
        Ok(TeamCommandArgs::Retract { peer, item_id }) => {
            AttachedTeamLine::Frame(ClientFrame::TeamRetractPreview {
                request,
                peer,
                item_id,
            })
        }
        Ok(TeamCommandArgs::Reply {
            item_id,
            text,
            verdict,
        }) => AttachedTeamLine::Frame(ClientFrame::ReplyToRecipientItem {
            item_id,
            text,
            verdict,
        }),
        Ok(TeamCommandArgs::Defer { item_id }) => {
            AttachedTeamLine::Frame(ClientFrame::DeferRecipientItem { item_id })
        }
        Ok(TeamCommandArgs::View { item_id }) => AttachedTeamLine::LocalView(item_id),
        Ok(TeamCommandArgs::Context { item_id: None, .. }) => {
            AttachedTeamLine::Frame(ClientFrame::InspectRecipientContext)
        }
        Ok(TeamCommandArgs::Context {
            item_id: Some(item_id),
            off,
        }) => AttachedTeamLine::Frame(ClientFrame::SetRecipientContext {
            item_id,
            selected: !off,
        }),
        Ok(TeamCommandArgs::Thread { peer, item_id }) => {
            AttachedTeamLine::Frame(ClientFrame::TeamThread {
                request,
                peer,
                item_id,
            })
        }
        Ok(TeamCommandArgs::Followup {
            peer,
            turn_id,
            text,
        }) => AttachedTeamLine::Frame(ClientFrame::TeamFollowup {
            request,
            peer,
            reply_to: turn_id,
            text,
        }),
        Ok(TeamCommandArgs::Status) => AttachedTeamLine::Frame(ClientFrame::TeamStatus { request }),
        Ok(TeamCommandArgs::Untrust(alias)) => {
            AttachedTeamLine::Frame(ClientFrame::RevokeSenderConsent { request, alias })
        }
        Err(error) => AttachedTeamLine::Refused(error),
    }
}

/// Paint the real attached shell once. Both the socket host and in-file
/// witnesses use this compositor, authoritative tab view and shared editor.
#[allow(clippy::too_many_arguments)]
fn paint_attached(
    f: &mut ratatui::Frame<'_>,
    state: &mut TuiState,
    conversation: &Conversation,
    streaming: &StreamingState,
    view: &crate::domain::models::view_state::ViewState,
    tab_render_state: &mut TabRenderState,
    tool_inputs: &HashMap<String, crate::adapters::daemon::protocol::ToolInputWithheld>,
    permission_mode: PermissionMode,
    attach_info: &AttachInfo,
) -> Option<chat_pane::RenderResult> {
    let read_only = attach_info.read_only;
    let area = f.area();
    let Some(layout) = crate::adapters::tui::layout::compute_layout(
        area,
        &state.theme,
        crate::adapters::tui::handlers::recipient_actions::pending_composition(state)
            .unwrap_or(&state.input_buffer),
        1,
        false,
        DensityMode::Focus,
    ) else {
        crate::adapters::tui::presentation::paint_too_small(f, area);
        return None;
    };
    let mut shell = crate::adapters::tui::presentation::Shell::new(area, &layout);
    // The attached client shows exactly one conversation: the thread frame's eligibility
    // facts (`thread_inspector::actions`) are read from it.
    shell.stage = Some((conversation, &[]));
    state
        .notifications
        .set_capacity(crate::adapters::tui::widgets::toast_layer::capacity_for(
            layout.chat_pane.height,
        ));
    let transcript_area = layout.chat_pane;
    let mut opaque =
        crate::adapters::tui::presentation::occlusion::OpaqueRects::new(transcript_area);
    tab_render_state.osc8_links =
        crate::adapters::tui::widgets::hyperlink::terminal_supports_osc8();
    if let Some(facts) = tab_render_state.launchpad.as_mut() {
        facts.terminal_width = state.terminal_width;
    }
    let result = chat_pane::render_attached(
        f,
        transcript_area,
        conversation,
        streaming,
        view,
        &state.theme,
        tab_render_state,
        &state.feedback_blocks,
        tool_inputs,
    );
    let pending_input =
        crate::adapters::tui::handlers::recipient_actions::pending_composition(state);
    let display_input = pending_input.unwrap_or(&state.input_buffer);
    let (display_cursor, display_scroll) = pending_input
        .map_or((state.cursor_position, state.input_scroll_offset), |text| {
            (text.chars().count(), usize::MAX)
        });
    input_box::render(
        f,
        layout.input_area,
        display_input,
        display_cursor,
        if state.modal_stack.is_empty() && state.pending_leader.is_none() {
            state.focus.clone()
        } else {
            FocusState::Chat
        },
        &state.theme,
        state.multiline_mode,
        display_scroll,
        if read_only {
            Some("read-only — can't send here")
        } else {
            None
        },
    );
    // Inline cards can leave fewer than five rows; paging must not skip visible content.
    state.viewport_height = transcript_area.height;
    let paint_status = |f: &mut ratatui::Frame<'_>| {
        status_bar::render(
            f,
            layout.status_bar,
            "(daemon)",
            None,
            &state.status,
            &state.theme,
            view.scroll_offset,
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
            attach_keys::status_hint(state).as_deref(),
            0,
            None,
            None,
            None,
            false,
            None,
            None,
            DensityMode::Focus,
            false,
            Some(attach_info),
            state.log_awareness.display,
            attached_notice_badges(state),
        );
    };
    shell.chat_pane = layout.chat_pane;
    let decision_painted = crate::adapters::tui::presentation::compose(
        f,
        state,
        &shell,
        crate::adapters::tui::keymap::Host::Attached,
        &attach_keys::log_facts(&conversation.id),
        &mut opaque,
        paint_status,
    );
    if decision_painted {
        crate::adapters::tui::decision_queue::note_painted(state);
    }
    state.total_content_height = result.total_content_height;
    state.scroll_snapshot = view.scroll_offset;
    state.auto_snapshot = matches!(view.mode, crate::domain::models::AnchorMode::Following);
    Some(result)
}

/// Connect to this workspace's daemon and run the rich attach TUI until the user
/// detaches (`Ctrl+X d` / `/detach`) or quits this client (guarded `q` / `Ctrl+Q` /
/// idle `Ctrl+C` / `/quit`), or the daemon closes the connection. `Esc`, `Ctrl+D`
/// and `Ctrl+C` never detach. The daemon and any in-flight turn keep running
/// across detach (AC4). `notifications_cfg` is the resolved `[notifications]`
/// config (Story 21.1 AC1): it sets the TTLs of the attached notice store.
pub async fn run_attached(
    workspace: &Path,
    notifications_cfg: &crate::domain::models::NotificationsConfig,
) -> Result<()> {
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
    let mut attached_tab =
        crate::domain::models::tab::TabState::from_conversation(0, seed_conversation(&snapshot));
    let crate::domain::models::tab::TabState {
        conversation,
        streaming,
        reducer,
        view_state: view,
        ..
    } = &mut attached_tab;
    let mut tool_inputs = AttachedToolInputs {
        reasons: snapshot
            .withheld_tool_inputs
            .iter()
            .map(|entry| (entry.tool_call_id.clone(), entry.withheld))
            .collect(),
        ..Default::default()
    };
    let clock = Arc::new(SystemClock::default());
    let mut permission_mode = snapshot.permission_mode;
    let attach_info = AttachInfo {
        read_only,
        channel_count: snapshot.channels.len(),
    };
    let mut session_manager = SessionManager::new(SessionState::Empty);
    let mut lifecycle =
        crate::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle::default();
    let mut approval_assemblies = HashMap::new();
    let mut attached_team_requests = AttachedTeamRequests {
        read_only,
        ..Default::default()
    };
    let mut attached_interrupt =
        crate::infrastructure::runtime::attach_interrupt::AttachedInterrupt::new(read_only);
    let mut attached_chrome = attach_keys::AttachedChrome::new();

    // ── Terminal ──
    let mut term = terminal::setup(false)?;
    let size = term.size()?;
    let mut state = TuiState::new(size.width, size.height);
    // Story 21.1 AC1/AC6: the attached notice store shares the loop's one clock.
    state.notifications = NotificationStore::from_config(notifications_cfg, clock.clone());
    state
        .notifications
        .set_capacity(crate::adapters::tui::widgets::toast_layer::capacity_for(
            size.height.saturating_sub(5),
        ));
    state.recipient_context_ids = snapshot.recipient_context_ids.iter().cloned().collect();
    crate::adapters::tui::decision_queue::sync_recipient_drafts(
        &mut state,
        conversation,
        read_only,
    );
    // Attach-time notices are store records visible in the first frame (AC6).
    if let Some(notice) = blocked_actions_notice(&snapshot) {
        notifications::warning(&mut state, NoticeSource::Attached, notice);
    }
    seed_mirror_waiting(&mut state, &snapshot, read_only);
    if read_only {
        // AC6 #2 — one-time, never-silent notice.
        notifications::info(
            &mut state,
            NoticeSource::Attached,
            "👁 Read-only. Another client is attached with write access. Detach the other to take over.",
        );
    }
    let mut tab_render_state = TabRenderState {
        workspace_root: Some(std::sync::Arc::from(workspace)),
        launchpad: Some(
            crate::infrastructure::runtime::launchpad_facts::attached_facts(
                workspace,
                state.terminal_width,
            ),
        ),
        ..TabRenderState::default()
    };
    // Story 19.16g — local, read-only journal access for the reminder and
    // `/team log`; nothing here creates a journal or sends a frame.
    let log_service = crate::infrastructure::transparency::TransparencyService::new(
        std::sync::Arc::new(
            crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                workspace,
            ),
        ),
        workspace.to_path_buf(),
    );
    let mut log_awareness =
        crate::infrastructure::transparency_awareness::LogAwarenessObserver::for_workspace(
            workspace,
        );
    let mut ticket_bridge = super::ticket_bridge::TicketBridge::new(workspace);
    // Attached clients render the same feedback blocks as the local TUI so
    // forwarded transparency room/failure events have the same visible result.

    use crossterm::event::{Event, EventStream, KeyEventKind};
    use futures::StreamExt;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let loop_result: Result<()> = loop {
        // Story 19.25 `AC3` — a card that left takes its leave-out overlay.
        crate::adapters::tui::handlers::recipient_actions::prune_retro_left_out(
            &mut state,
            conversation,
        );
        // Story 21.1 A4: every frame starts by archiving the toasts whose deadline
        // has been reached, so no frame at or after a deadline paints them.
        notifications::begin_draw(&mut state);
        let mut rendered = None;
        let drawn = term.draw(|f| {
            rendered = paint_attached(
                f,
                &mut state,
                conversation,
                streaming,
                view,
                &mut tab_render_state,
                &tool_inputs.reasons,
                permission_mode,
                &attach_info,
            );
        });
        if rendered.is_some() {
            after_attached_draw(&mut state, drawn.is_ok());
        }

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        let mut key_ctx = attach_keys::KeyCtx {
                            workspace,
                            read_only,
                            state: &mut state,
                            conversation,
                            streaming,
                            frame_tx: &frame_tx,
                            team_requests: &mut attached_team_requests,
                            tab: &mut tab_render_state,
                            view,
                            interrupt: &attached_interrupt,
                            driver: &driver,
                            lifecycle: &mut lifecycle,
                            session_manager: &mut session_manager,
                            log_service: &log_service,
                            chrome: &mut attached_chrome,
                        };
                        match attach_keys::handle_attached_key(&mut key_ctx, key).await {
                            attach_keys::KeyFlow::Continue => {}
                            attach_keys::KeyFlow::Detach | attach_keys::KeyFlow::Quit => break Ok(()),
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        let pending = false;
                        attached_chrome.paste(&mut state, &text, pending, workspace).await;
                    }
                    Some(Ok(event @ Event::Mouse(_))) => {
                        let _ = attach_keys::handle_attached_mouse(&mut state, view, &event);
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
                        Ok(Some(DaemonFrame::PeerRowOperation(operation))) => {
                            let refresh_item = state.thread_inspector.as_ref().is_some_and(|inspector| {
                                let (row_id, new_control) = match operation.as_ref() {
                                    crate::domain::models::PeerRowOperation::Upsert { message, control, .. } =>
                                        (message.id.as_str(), control.as_ref()),
                                    crate::domain::models::PeerRowOperation::Remove { row_id, .. } =>
                                        (row_id.as_str(), None),
                                };
                                let control = new_control.or_else(|| conversation.peer_row_controls.get(row_id));
                                match &inspector.target {
                                    crate::adapters::tui::state::ThreadTarget::Item(address) => control
                                        .is_some_and(|control| control.item_address.as_ref() == Some(address)),
                                    crate::adapters::tui::state::ThreadTarget::Node(node) => control
                                        .is_some_and(|control| &control.node_id == node),
                                    crate::adapters::tui::state::ThreadTarget::Sent { .. }
                                    | crate::adapters::tui::state::ThreadTarget::Log { .. } => false,
                                }
                            });
                            if apply_peer_row_operation(conversation, Arc::unwrap_or_clone(operation)) {
                                crate::adapters::tui::decision_queue::sync_recipient_drafts(&mut state, conversation, read_only);
                                if refresh_item {
                                    super::thread_open::reconcile_item_thread(&mut state, conversation);
                                }
                                state.needs_redraw = true;
                            }
                        }
                        Ok(Some(DaemonFrame::Event(raw))) => {
                            tool_inputs.apply(
                                raw,
                                None,
                                conversation,
                                streaming,
                                reducer,
                                &mut state,
                                &mut permission_mode,
                                &*clock,
                            );
                        }
                    Ok(Some(DaemonFrame::ToolEventWithheld { event, withheld })) => {
                        tool_inputs.apply(
                            event, Some(withheld), conversation, streaming, reducer,
                            &mut state, &mut permission_mode, &*clock,
                        );
                    }
                    Ok(Some(DaemonFrame::History { messages, withheld_tool_inputs: withheld, .. })) => {
                        for entry in withheld {
                            tool_inputs.reasons.insert(entry.tool_call_id, entry.withheld);
                        }
                        let existing: std::collections::HashSet<_> =
                            conversation.messages.iter().map(|row| row.id.clone()).collect();
                        let mut earlier: Vec<_> = messages.into_iter()
                            .filter(|row| !existing.contains(&row.id)).collect();
                        earlier.append(&mut conversation.messages);
                        conversation.messages = earlier;
                        state.needs_redraw = true;
                    }
                    Ok(Some(frame @ (DaemonFrame::FrameTooLarge { .. }
                        | DaemonFrame::Error(_)
                        | DaemonFrame::ActiveTurn { .. }
                        | DaemonFrame::InterruptResult { .. }))) => {
                        apply_notice_frame(&mut state, &mut attached_interrupt, &frame);
                    }
                    Ok(Some(frame @ DaemonFrame::ApprovalRequest { .. }))
                    | Ok(Some(frame @ DaemonFrame::ApprovalInputChunk { .. }))
                    | Ok(Some(frame @ DaemonFrame::ApprovalInputComplete { .. }))
                    | Ok(Some(frame @ DaemonFrame::ApprovalClosed { .. }))
                    | Ok(Some(frame @ DaemonFrame::ApprovalRefused { .. })) => {
                        apply_attached_approval_frame(&mut state, &mut approval_assemblies, frame, read_only);
                    }
                    Ok(Some(frame @ DaemonFrame::ConsolidationProposed { .. })) => {
                        apply_attached_consolidation_frame(&mut state, frame, read_only);
                    }
                    Ok(Some(frame @ DaemonFrame::TeamProposal { .. }))
                    | Ok(Some(frame @ DaemonFrame::TeamProposalClosed { .. })) => {
                        apply_attached_proposal_frame(
                            &mut state, read_only, &mut attached_team_requests,
                            &conversation.id, frame,
                        );
                    }
                    Ok(Some(DaemonFrame::Detached)) => break Ok(()),
                    Ok(Some(frame @ DaemonFrame::TeamSendStarted { .. }))
                    | Ok(Some(frame @ DaemonFrame::TeamSendSettled { .. }))
                    | Ok(Some(frame @ DaemonFrame::TeamViewChunk { .. }))
                    | Ok(Some(frame @ DaemonFrame::TeamViewDone { .. }))
                    | Ok(Some(frame @ DaemonFrame::TeamRetractPreviewed { .. }))
                    | Ok(Some(frame @ DaemonFrame::TeamRetractAnswered { .. }))
                    | Ok(Some(frame @ DaemonFrame::TeamStatusResult { .. }))
                    | Ok(Some(frame @ DaemonFrame::SenderConsentRevoked { .. })) => {
                        if !read_only || !matches!(&frame, DaemonFrame::TeamRetractPreviewed { .. }) {
                            let confirmation_busy =
                                crate::adapters::tui::decision_queue::higher_confirmation_presenting(&state)
                                    || state.decisions.presenting(crate::adapters::tui::decision_queue::DecisionKind::Consent);
                            let _ = apply_attached_team_frame(
                                &mut state, &mut attached_team_requests, frame, confirmation_busy, &frame_tx,
                            );
                        }
                    }
                    Ok(Some(DaemonFrame::RecipientActionResult { action, target, outcome })) => {
                        match target {
                            crate::adapters::daemon::protocol::RecipientActionTarget::Request { request } => {
                                let _ = apply_attached_team_refusal(
                                    &mut state, &mut attached_team_requests, action, request, outcome,
                                );
                            }
                            target if matches!(
                                action,
                                crate::adapters::daemon::protocol::RecipientAction::TeamSend
                                    | crate::adapters::daemon::protocol::RecipientAction::TeamBoard
                                    | crate::adapters::daemon::protocol::RecipientAction::TeamStatus
                                    | crate::adapters::daemon::protocol::RecipientAction::RevokeSenderConsent
                            ) => {
                                tracing::warn!(?action, ?target, "attach: team refusal used a non-request target");
                            }
                            target => crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                                &mut state, action, target, outcome,
                            ),
                        }
                    }
                    Ok(Some(DaemonFrame::RecipientContextState {
                        selected_item_ids, inventory, show_inventory,
                    })) => {
                        crate::adapters::tui::handlers::recipient_actions::apply_context_state(
                            &mut state, selected_item_ids, inventory, show_inventory,
                        );
                    }
                    // AttachAck/History are not expected post-handshake; ignore.
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        notifications::warning(
                            &mut state,
                            NoticeSource::Daemon,
                            "— daemon closed the connection —",
                        );
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
            _ = tick.tick() => {
                crate::adapters::tui::decision_queue::tick(&mut state);
                log_awareness.tick(&mut state.log_awareness);
                ticket_bridge.observe(&mut state, &log_awareness,
                    crate::adapters::tui::decision_queue::Responder::Attached, read_only,
                    #[cfg(feature = "mcp")] &[]).await;
                if let Some(counts) = log_awareness.reply_counts() {
                    crate::adapters::tui::decision_queue::reply::journal_counts(&mut state, counts);
                }
                // Story 21.1 AC6: attached notices expire — the tick requests the
                // deadline redraw (the draw's `begin_draw` does the sweep).
                notifications::tick(&mut state);
            }
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
#[path = "attach_loop/inspect_hint_tests.rs"]
mod inspect_hint_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::thread_inspector::{self, ThreadAction};
    use crate::domain::models::{NoticeLevel, StopReason, StreamChunk};
    use std::time::Instant;
    use tokio::net::UnixStream;

    fn attached_screen(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn story21_8_attached_sent_thread_completion_refusal_and_lifetime() {
        use crate::adapters::daemon::protocol::{
            ProtocolError, RecipientAction, RecipientActionOutcome,
        };
        use crate::adapters::tui::keymap::Key;
        use crate::adapters::tui::state::{ThreadAccess, ThreadTarget};
        for read_only in [false, true] {
            let mut rig = attach_keys::tests::Rig::new(read_only);
            rig.team_requests.read_only = read_only;
            let open = |rig: &mut attach_keys::tests::Rig, request: &str| {
                assert!(enqueue_attached_team_frame(
                    &mut rig.state,
                    &mut rig.team_requests,
                    ClientFrame::TeamThread {
                        request: request.into(),
                        peer: "B".into(),
                        item_id: "ri_a".into()
                    },
                    &rig.tx,
                ));
                assert!(
                    matches!(rig.frames().as_slice(), [ClientFrame::TeamThread { request: id, .. }] if id == request)
                );
            };
            let finish = |rig: &mut attach_keys::tests::Rig, request: &str, text: &str| {
                for frame in [
                    DaemonFrame::TeamViewChunk {
                        request: request.into(),
                        sequence: 0,
                        view: text.into(),
                    },
                    DaemonFrame::TeamViewDone {
                        request: request.into(),
                        chunks: 1,
                    },
                ] {
                    apply_attached_team_frame(
                        &mut rig.state,
                        &mut rig.team_requests,
                        frame,
                        false,
                        &rig.tx,
                    );
                }
            };
            open(&mut rig, "first");
            let thread = rig.state.thread_inspector.as_ref().unwrap();
            assert!(
                matches!(&thread.target, ThreadTarget::Sent { peer, item } if peer == "B" && item == "ri_a")
            );
            assert_eq!(
                thread.access,
                if read_only {
                    ThreadAccess::ReadOnly
                } else {
                    ThreadAccess::Writer
                }
            );
            assert!(thread.sent.as_ref().unwrap().loading);
            let loading = attached_screen(&rig.paint());
            assert!(
                loading.contains("Thread · sent · B") && loading.contains("reading…"),
                "{loading}"
            );
            finish(
                &mut rig,
                "first",
                "Credential: verified.\n  3 earlier turns not shown — the full thread is on B\n  you  ri_a\n      hello \u{1b}red\u{202e} tail",
            );
            let painted = attached_screen(&rig.paint());
            for text in [
                "Credential: verified.",
                "3 earlier turns not shown",
                "you",
                "tail",
                "\\u{202e}",
            ] {
                assert!(painted.contains(text), "{text}: {painted}");
            }
            assert!(
                !painted.contains('\u{1b}') && !painted.contains('\u{202e}'),
                "{painted}"
            );
            assert!(
                !rig.state
                    .thread_inspector
                    .as_ref()
                    .unwrap()
                    .sent
                    .as_ref()
                    .unwrap()
                    .loading
            );
            open(&mut rig, "old");
            open(&mut rig, "new");
            finish(&mut rig, "old", "stale completion");
            assert!(attached_screen(&rig.paint()).contains("reading…"));
            finish(&mut rig, "new", "new completion");
            assert!(attached_screen(&rig.paint()).contains("new completion"));
            open(&mut rig, "refusal");
            apply_attached_team_refusal(
                &mut rig.state,
                &mut rig.team_requests,
                RecipientAction::TeamThread,
                "refusal".into(),
                RecipientActionOutcome::Refused {
                    error: ProtocolError::Internal("not found".into()),
                },
            );
            let refused = attached_screen(&rig.paint());
            assert!(
                refused.contains("[daemon error]") && refused.contains("not found"),
                "{refused}"
            );
            open(&mut rig, "closed");
            rig.key(Key::Esc).await;
            assert!(rig.state.modal_stack.is_empty());
            let blocks = rig.state.feedback_blocks.len();
            finish(&mut rig, "closed", "must not return");
            assert!(rig.state.modal_stack.is_empty());
            assert_eq!(rig.state.feedback_blocks.len(), blocks);
            assert!(!attached_screen(&rig.paint()).contains("must not return"));
        }
    }

    #[tokio::test]
    async fn story21_8_attached_hostile_sent_and_log_display() {
        use crate::adapters::tui::{keymap::Key, thread_body};
        use crate::domain::models::{Direction, JournalRecord, PeerId, RejectReason, RoomEvent};
        use crate::domain::services::transparency::render_export;
        use crate::infrastructure::runtime::thread_open;
        use crate::infrastructure::subagent::{NodeJournal, node_journal::WorkspaceJournalReader};
        use crate::infrastructure::transparency::TransparencyService;

        let mut rig = attach_keys::tests::Rig::new(false);
        rig.state.terminal_width = 160;
        rig.state.terminal_height = 40;
        let base = "thread · ada · ri_open\n  you       ri_open\n      body";
        for (suffix, mine, retracted, rail) in [
            ("", true, false, true),
            (
                "\n  ada       other\n        you       ri_open  [retracted 99:99]",
                true,
                false,
                true,
            ),
            ("\n  ada       ri_open\n      reply", false, false, false),
            (
                "\n  ada       ri_open  [retracted 99:99]\n      reply",
                false,
                false,
                false,
            ),
            ("\n  you       ri_open\n      reply", false, false, false),
            (
                "\n  ada  ri_open       other\n      reply",
                false,
                false,
                false,
            ),
        ] {
            assert!(enqueue_attached_team_frame(
                &mut rig.state,
                &mut rig.team_requests,
                ClientFrame::TeamThread {
                    request: "sent".into(),
                    peer: "ada".into(),
                    item_id: "ri_open".into()
                },
                &rig.tx
            ));
            assert!(matches!(
                rig.frames().as_slice(),
                [ClientFrame::TeamThread { .. }]
            ));
            for frame in [
                DaemonFrame::TeamViewChunk {
                    request: "sent".into(),
                    sequence: 0,
                    view: format!("{base}{suffix}"),
                },
                DaemonFrame::TeamViewDone {
                    request: "sent".into(),
                    chunks: 1,
                },
            ] {
                apply_attached_team_frame(
                    &mut rig.state,
                    &mut rig.team_requests,
                    frame,
                    false,
                    &rig.tx,
                );
            }
            let facts = rig
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .sent
                .as_ref()
                .unwrap();
            assert_eq!((facts.mine, facts.retracted), (mine, retracted), "{suffix}");
            let screen = attached_screen(&rig.paint());
            assert_eq!(screen.contains("[x] Retract"), rail, "{suffix}: {screen}");
        }
        let generation = thread_open::open_sent(
            &mut rig.state,
            crate::adapters::tui::state::ThreadAccess::Writer,
            "ada",
            "ri_open",
            "retracted",
        );
        assert!(thread_open::fill_sent(
            &mut rig.state,
            "retracted",
            generation,
            "thread · ada · ri_open\n  you       ri_open  [retracted 12:34]\n      body"
        ));
        assert!(
            rig.state
                .thread_inspector
                .as_ref()
                .unwrap()
                .sent
                .as_ref()
                .unwrap()
                .retracted
        );
        assert!(!attached_screen(&rig.paint()).contains("[x] Retract"));
        rig.key(Key::Esc).await;

        let journal = NodeJournal::open_workspace(rig.workspace()).await.unwrap();
        journal
            .append_batch(vec![JournalRecord::Room(
                RoomEvent::RemoteEnvelopeRejected {
                    rail: None,
                    peer: PeerId::from_public_key(&[7; 32]).unwrap(),
                    reason: RejectReason::Policy {
                        detail: "سلام\u{202e}A\u{2066}B\u{1b}".into(),
                    },
                    direction: Direction::Inbound,
                    task: None,
                },
            )])
            .await
            .unwrap();
        let journal_bytes = std::fs::read(journal.path()).unwrap();
        let service = TransparencyService::new(
            std::sync::Arc::new(WorkspaceJournalReader::open_workspace(rig.workspace())),
            rig.workspace().to_path_buf(),
        );
        for command in ["/team log", "/team log --json"] {
            let before = render_export(&service.report().await.unwrap().rows);
            assert!(before.contains('\u{202e}') && before.contains('\u{2066}'));
            rig.paste(command).await;
            rig.key(Key::Enter).await;
            assert!(rig.frames().is_empty());
            let inspector = rig.state.thread_inspector.as_ref().unwrap();
            let lines = thread_body::build(inspector, &rig.state.theme, 150).lines;
            let spans: String = lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
                .collect();
            for text in ["\\u{202e}", "\\u{2066}", "سلام"] {
                assert!(spans.contains(text), "{command}: {spans:?}");
            }
            assert!(!spans.chars().any(|c| c == '\u{1b}'
                || ('\u{202a}'..='\u{202e}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c)));
            let terminal = rig.paint();
            let screen = attached_screen(&terminal);
            for text in ["\\u{202e}", "\\u{2066}", "سلام"] {
                assert!(screen.contains(text), "{command}: {screen}");
            }
            assert!(
                !terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .flat_map(|cell| cell.symbol().chars())
                    .any(|c| c == '\u{1b}'
                        || ('\u{202a}'..='\u{202e}').contains(&c)
                        || ('\u{2066}'..='\u{2069}').contains(&c))
            );
            assert_eq!(before, render_export(&service.report().await.unwrap().rows));
            assert_eq!(journal_bytes, std::fs::read(journal.path()).unwrap());
            rig.key(Key::Esc).await;
        }
    }

    #[tokio::test]
    async fn story21_8_attached_log_scrolling_filter_and_leader() {
        use crate::adapters::tui::{keymap::Key, state::ThreadTarget};
        let mut rig = attach_keys::tests::Rig::new(false);
        rig.key(Key::Ctrl('x')).await;
        rig.key(Key::Char('l')).await;
        assert!(matches!(
            rig.state.thread_inspector.as_ref().unwrap().target,
            ThreadTarget::Log { filtered: false }
        ));
        assert!(attached_screen(&rig.paint()).contains("A2A log"));
        rig.key(Key::Esc).await;
        let workspace = tempfile::tempdir().unwrap();
        crate::infrastructure::transparency_awareness::test_support::append_rows(
            workspace.path(),
            25,
        )
        .await;
        let service = crate::infrastructure::transparency::TransparencyService::new(
            std::sync::Arc::new(crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(workspace.path())),
            workspace.path().to_path_buf(),
        );
        let args = crate::adapters::tui::handlers::team_command::TeamLogArgs::default();
        assert!(
            crate::infrastructure::runtime::transparency_bridge::attached_team_log(
                &service,
                &mut rig.state,
                &args
            )
            .await
            .is_empty()
        );
        assert!(rig.state.thread_inspector.as_ref().unwrap().turns.len() >= 25);
        assert!(
            !rig.state
                .thread_inspector
                .as_ref()
                .unwrap()
                .content
                .contains("showing the 20 most recent")
        );
        assert!(rig.state.pending_log_visit.is_some());
        assert!(attached_screen(&rig.paint()).contains("A2A log"));
        assert!(rig.state.pending_log_visit.is_none());
        let json = crate::adapters::tui::handlers::team_command::TeamLogArgs {
            json: true,
            ..Default::default()
        };
        assert!(
            crate::infrastructure::runtime::transparency_bridge::attached_team_log(
                &service,
                &mut rig.state,
                &json
            )
            .await
            .is_empty()
        );
        assert!(rig.state.pending_log_visit.is_some());
        assert!(attached_screen(&rig.paint()).contains("A2A log"));
        assert!(rig.state.pending_log_visit.is_none());
        let filtered_args = crate::adapters::tui::handlers::team_command::TeamLogArgs {
            filter: Some("direction=inbound".into()),
            ..Default::default()
        };
        assert!(
            crate::infrastructure::runtime::transparency_bridge::attached_team_log(
                &service,
                &mut rig.state,
                &filtered_args
            )
            .await
            .is_empty()
        );
        assert!(rig.state.pending_log_visit.is_none());
        assert!(attached_screen(&rig.paint()).contains("A2A log · filtered"));
        assert!(rig.state.pending_log_visit.is_none());
        rig.key(Key::Esc).await;
        rig.state.log_awareness.presented.clear();

        let rows: Vec<String> = (0..25).map(|n| format!("log-row-{n:02}")).collect();
        super::super::thread_open::open_log(&mut rig.state, false, &rows);
        let visit = crate::domain::models::LogVisitCandidate {
            seen_through: 25,
            reset_revision: 0,
        };
        rig.state.pending_log_visit = Some(visit);
        let mut seen = String::new();
        for _ in 0..30 {
            seen.push_str(&attached_screen(&rig.paint()));
            rig.key(Key::Char('j')).await;
        }
        for n in 0..25 {
            assert!(
                seen.contains(&format!("log-row-{n:02}")),
                "missing row {n}: {seen}"
            );
        }
        assert_eq!(rig.state.log_awareness.presented, [visit]);
        assert!(rig.state.pending_log_visit.is_none());
        super::super::thread_open::open_log(&mut rig.state, true, &rows);
        rig.state.pending_log_visit = Some(visit);
        let filtered = attached_screen(&rig.paint());
        assert!(filtered.contains("A2A log · filtered"), "{filtered}");
        assert_eq!(rig.state.pending_log_visit, Some(visit));
        assert_eq!(rig.state.log_awareness.presented, [visit]);
        rig.key(Key::Esc).await;
        assert!(rig.state.modal_stack.is_empty());
        rig.paint();
        assert_eq!(rig.state.log_awareness.presented, [visit]);
        assert!(rig.frames().is_empty());
    }

    #[test]
    fn story21_8_attached_send_summary_settles_in_place_and_notices_once() {
        use crate::adapters::daemon::protocol::{
            TeamSendCause, TeamSendCauseKind, TeamSendOutcome,
        };
        use crate::adapters::tui::handlers::team_command::TEAM_SEND_RETRO_NOTE;
        use crate::domain::models::SemanticMessageType;
        let mut rig = attach_keys::tests::Rig::new(false);
        rig.state.terminal_width = 60;
        let send = ClientFrame::TeamSend {
            request: "send".into(),
            recipients: vec!["alice".into(), "bob".into(), "carol".into()],
            text: "hi".into(),
            message_type: Some(SemanticMessageType::RetroRequest),
            subject: None,
            stories: None,
            holders_of: None,
        };
        assert!(enqueue_attached_team_frame(
            &mut rig.state,
            &mut rig.team_requests,
            send,
            &rig.tx
        ));
        assert!(
            matches!(rig.frames().as_slice(), [ClientFrame::TeamSend { request, .. }] if request == "send")
        );
        assert_eq!(rig.state.feedback_blocks.len(), 1);
        assert_eq!(
            rig.state.feedback_blocks["team-send-1"].message,
            "Send · retro_request · alice sending… · bob sending… · carol sending…"
        );
        let painted = rig.paint();
        let screen = attached_screen(&painted);
        for alias in ["alice", "bob", "carol"] {
            assert!(screen.contains(alias), "{screen}");
        }
        let layout = crate::adapters::tui::layout::compute_layout(
            ratatui::layout::Rect::new(0, 0, 60, 24),
            &rig.state.theme,
            &rig.state.input_buffer,
            1,
            false,
            DensityMode::Focus,
        )
        .unwrap();
        let lines = crate::adapters::tui::widgets::feedback_block::render_feedback_lines(
            &rig.state.feedback_blocks["team-send-1"],
            layout.chat_pane.width,
            &rig.state.theme,
        );
        assert!(lines.len() > 1);
        assert_eq!(rig.state.total_content_height, lines.len());
        let painted_rows = (layout.chat_pane.y..layout.chat_pane.bottom())
            .filter(|&y| painted.backend().buffer()[(layout.chat_pane.x, y)].symbol() == "│")
            .count();
        assert_eq!(painted_rows, lines.len());
        apply_attached_team_frame(
            &mut rig.state,
            &mut rig.team_requests,
            DaemonFrame::TeamSendStarted {
                request: "send".into(),
                action: crate::domain::models::SendActionId::mint(),
                recipients: vec!["alice".into(), "bob".into(), "carol".into(), "dave".into()],
            },
            false,
            &rig.tx,
        );
        assert!(
            rig.state.feedback_blocks["team-send-1"]
                .message
                .contains("dave sending…")
        );
        let settle = |rig: &mut attach_keys::tests::Rig, index, outcome| {
            apply_attached_team_frame(
                &mut rig.state,
                &mut rig.team_requests,
                DaemonFrame::TeamSendSettled {
                    request: "send".into(),
                    index,
                    outcome,
                },
                false,
                &rig.tx,
            );
            assert_eq!(rig.state.feedback_blocks.len(), 1);
            assert!(
                !rig.state.feedback_blocks["team-send-1"]
                    .message
                    .contains('\n')
            );
        };
        settle(&mut rig, 0, TeamSendOutcome::Delivered);
        assert!(
            rig.state.feedback_blocks["team-send-1"]
                .message
                .contains("● alice sent")
        );
        let unreachable = TeamSendOutcome::Unreachable {
            cause: Some(TeamSendCause {
                kind: TeamSendCauseKind::Ratified,
                text: "off\u{1b}line".into(),
            }),
        };
        settle(&mut rig, 1, unreachable.clone());
        settle(&mut rig, 1, unreachable);
        settle(&mut rig, 2, TeamSendOutcome::Delivered);
        settle(&mut rig, 3, TeamSendOutcome::Delivered);
        let records: Vec<_> = rig.state.notifications.records().collect();
        assert_eq!(
            records
                .iter()
                .filter(|n| n.message == "bob unreachable: offline")
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|n| n.message == TEAM_SEND_RETRO_NOTE)
                .count(),
            1
        );
        assert!(
            records
                .iter()
                .any(|n| n.message == "bob unreachable: offline"
                    && n.severity == NoticeSeverity::Warning)
        );
        assert!(
            records
                .iter()
                .any(|n| n.message == TEAM_SEND_RETRO_NOTE && n.severity == NoticeSeverity::Info)
        );
        assert!(
            rig.state.feedback_blocks["team-send-1"]
                .message
                .contains("⚠ bob unreachable")
        );
        assert!(
            rig.state.feedback_blocks["team-send-1"]
                .message
                .contains("● carol sent")
        );
    }

    fn attached_frame_record(
        rig: &mut attach_keys::tests::Rig,
        stage: &str,
        read_only: bool,
    ) -> String {
        let mut terminal = rig.paint();
        let screen = attached_screen(&terminal);
        let buffer = terminal.backend().buffer();
        let layout = crate::adapters::tui::layout::compute_layout(
            buffer.area,
            &rig.state.theme,
            &rig.state.input_buffer,
            1,
            false,
            DensityMode::Focus,
        )
        .unwrap();
        assert!(
            screen
                .lines()
                .nth(usize::from(layout.status_bar.y))
                .unwrap()
                .contains("Ready"),
            "{screen}"
        );
        assert_eq!(
            terminal.backend().cursor_visible(),
            rig.state.modal_stack.is_empty() && rig.state.pending_leader.is_none()
        );
        if terminal.backend().cursor_visible() {
            let cursor = terminal.get_cursor_position().unwrap();
            assert_eq!(
                (cursor.x, cursor.y),
                (
                    rig.state.cursor_position as u16 + 1,
                    rig.state.terminal_height - 2
                )
            );
        }
        if let Some(path) = std::env::var_os("RUSTAIN_STORY_21_4_FRAME_DIR") {
            let path = std::path::PathBuf::from(path);
            assert!(path.is_absolute());
            assert!(
                !path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            );
            std::fs::create_dir_all(&path).unwrap();
            let path = std::fs::canonicalize(path).unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            let reversed_cells: Vec<_> = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .enumerate()
                .filter(|(_, cell)| cell.modifier.contains(ratatui::style::Modifier::REVERSED))
                .map(|(i, _)| {
                    [
                        i % usize::from(rig.state.terminal_width),
                        i / usize::from(rig.state.terminal_width),
                    ]
                })
                .collect();
            let host = if read_only { "readonly" } else { "attached" };
            let record = serde_json::json!({
                "width": rig.state.terminal_width, "height": rig.state.terminal_height,
                "host": host, "stage": stage, "full": false, "screen": screen,
                "draft": rig.state.input_buffer, "cursor": [cursor.x, cursor.y],
                "cursor_visible": terminal.backend().cursor_visible(), "reversed_cells": reversed_cells,
            });
            let name = format!(
                "c-{host}-{}x{}-{stage}.json",
                rig.state.terminal_width, rig.state.terminal_height
            );
            std::fs::write(path.join(name), serde_json::to_vec(&record).unwrap()).unwrap();
        }
        screen
    }

    fn attached_occlusion_rig(
        width: u16,
        height: u16,
        read_only: bool,
        prefix_rows: usize,
    ) -> attach_keys::tests::Rig {
        let mut rig = attach_keys::tests::Rig::new(read_only);
        rig.state.terminal_width = width;
        rig.state.terminal_height = height;
        for row in 0..prefix_rows {
            rig.domain_tab
                .conversation
                .messages
                .push(system_message(format!("occlusion prefix {row}")));
        }
        super::super::thread_open::open_log(
            &mut rig.state,
            false,
            &["M12-ATTACHED-VISIBLE".into()],
        );
        rig.state.pending_log_visit = Some(crate::domain::models::LogVisitCandidate {
            seen_through: 41,
            reset_revision: 0,
        });
        rig
    }

    /// M12: one real feedback row is visible before/after Help, but a successful
    /// draw of the fully covering shared frame must not commit its pending visit.
    #[tokio::test]
    async fn story21_4_attached_fully_occluded_block_is_not_presented() {
        use crate::adapters::tui::{
            keymap::Key,
            presentation::{self, Surface},
        };
        for read_only in [false, true] {
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                let mut rig = attached_occlusion_rig(width, height, read_only, 3);
                let visit = rig.state.pending_log_visit.unwrap();
                let before = attached_screen(&rig.paint());
                let rows: Vec<_> = before
                    .lines()
                    .enumerate()
                    .filter_map(|(row, text)| {
                        text.contains("M12-ATTACHED-VISIBLE").then_some(row as u16)
                    })
                    .collect();
                assert_eq!(
                    rows.len(),
                    1,
                    "independent exactly-one-row visible control: {before}"
                );
                assert_eq!(rig.state.log_awareness.presented, [visit]);
                assert!(rig.state.pending_log_visit.is_none());
                let layout = crate::adapters::tui::layout::compute_layout(
                    ratatui::layout::Rect::new(0, 0, width, height),
                    &rig.state.theme,
                    &rig.state.input_buffer,
                    1,
                    false,
                    DensityMode::Focus,
                )
                .unwrap();
                let rect =
                    presentation::geometry::frame_rect(presentation::geometry::modal_region(
                        &layout,
                        ratatui::layout::Rect::new(0, 0, width, height),
                    ));
                assert!(
                    rows[0] >= rect.y && rows[0] < rect.bottom(),
                    "the observed row must actually lie under the production frame"
                );
                rig.state.log_awareness.presented.clear();
                rig.state.pending_log_visit = Some(visit);
                rig.key(Key::Ctrl('x')).await;
                rig.key(Key::Char('?')).await;
                assert_eq!(rig.state.modal_stack.top().unwrap().surface, Surface::Help);
                let covered = attached_screen(&rig.paint());
                assert!(covered.contains("Help"), "{covered}");
                assert!(!covered.contains("M12-ATTACHED-VISIBLE"), "{covered}");
                assert_eq!(rig.state.pending_log_visit, Some(visit));
                assert!(rig.state.log_awareness.presented.is_empty());
                rig.key(Key::Esc).await;
                assert_eq!(
                    rig.state.modal_stack.top().unwrap().surface,
                    Surface::ThreadInspector
                );
                let returned = attached_screen(&rig.paint());
                assert!(returned.contains("M12-ATTACHED-VISIBLE"), "{returned}");
                assert_eq!(rig.state.log_awareness.presented, [visit]);
                assert!(rig.state.pending_log_visit.is_none());
                assert_eq!(rig.domain_tab.view_state.scroll_offset, 0);
                assert_eq!(
                    rig.domain_tab.view_state.mode,
                    crate::domain::models::AnchorMode::Following
                );
                assert!(
                    rig.frames().is_empty(),
                    "paint and local Help send no wire actions"
                );
            }
        }
    }

    #[test]
    fn story21_4_r3_attached_indicator_does_not_present_the_replaced_row() {
        for read_only in [false, true] {
            let mut rig = attached_occlusion_rig(80, 24, read_only, 0);
            let old = rig.state.pending_log_visit.unwrap();
            let newer = crate::domain::models::LogVisitCandidate {
                seen_through: old.seen_through + 1,
                reset_revision: old.reset_revision,
            };
            super::super::thread_open::open_log(&mut rig.state, false, &["R3-new-log-row".into()]);
            rig.state.pending_log_visit = Some(newer);
            let screen = attached_screen(&rig.paint());
            assert!(screen.contains("R3-new-log-row"), "{screen}");
            assert!(!screen.contains("M12-ATTACHED-VISIBLE"), "{screen}");
            assert_eq!(rig.state.log_awareness.presented, [newer]);
            assert!(rig.state.pending_log_visit.is_none());
            assert!(rig.frames().is_empty());
        }
    }

    #[test]
    fn story21_8_attached_streaming_indicator_still_covers_stage_feedback() {
        use crate::adapters::tui::views::{self, ViewKind};
        use crate::domain::models::{AnchorMode, FeedbackLevel};
        for read_only in [false, true] {
            for (streaming, surviving) in [(true, false), (true, true), (false, false)] {
                let mut rig = attach_keys::tests::Rig::new(read_only);
                let pane = crate::adapters::tui::layout::compute_layout(
                    ratatui::layout::Rect::new(0, 0, 80, 24),
                    &rig.state.theme,
                    &rig.state.input_buffer,
                    1,
                    false,
                    DensityMode::Focus,
                )
                .unwrap()
                .chat_pane;
                let keep = if surviving { 2 } else { 1 };
                views::show_view(
                    &mut rig.state.feedback_blocks,
                    ViewKind::TeamStatus,
                    "r3-row",
                    FeedbackLevel::Info,
                    "R3-first-feedback-row\nR3-second-feedback-row",
                );
                rig.domain_tab.view_state.mode = AnchorMode::Reading;
                rig.domain_tab.view_state.scroll_offset = usize::MAX;
                rig.domain_tab.streaming.is_streaming = streaming;
                let visible = attached_screen(&rig.paint());
                let origin = visible
                    .lines()
                    .position(|row| row.contains("R3-first-feedback-row"))
                    .unwrap();
                let target = pane.bottom() - keep;
                assert!(origin < target as usize, "{visible}");
                views::show_view(
                    &mut rig.state.feedback_blocks,
                    ViewKind::TeamStatus,
                    "r3-padding",
                    FeedbackLevel::Info,
                    vec!["padding"; target as usize - origin].join("\n"),
                );
                rig.state.feedback_blocks.remove("r3-row");
                views::show_view(
                    &mut rig.state.feedback_blocks,
                    ViewKind::TeamStatus,
                    "r3-row",
                    FeedbackLevel::Info,
                    "R3-first-feedback-row\nR3-second-feedback-row",
                );
                let screen = attached_screen(&rig.paint());
                if streaming {
                    assert!(
                        screen
                            .lines()
                            .nth((pane.bottom() - 1) as usize)
                            .unwrap()
                            .contains("New content below"),
                        "{screen}"
                    );
                }
                if streaming && !surviving {
                    assert!(!screen.contains("R3-first-feedback-row"), "{screen}");
                    rig.domain_tab.streaming.is_streaming = false;
                    assert!(attached_screen(&rig.paint()).contains("R3-first-feedback-row"));
                } else if surviving {
                    assert!(
                        screen
                            .lines()
                            .nth(target as usize)
                            .unwrap()
                            .contains("R3-first-feedback-row"),
                        "{screen}"
                    );
                }
                assert!(rig.frames().is_empty());
            }
        }
    }

    #[test]
    fn story21_4_attached_absent_empty_message_and_minimum_draw_controls() {
        for read_only in [false, true] {
            let mut rig = attached_occlusion_rig(80, 24, read_only, 0);
            let visit = rig.state.pending_log_visit.unwrap();
            super::super::thread_open::open_log(&mut rig.state, false, &[String::new()]);
            let empty = attached_screen(&rig.paint());
            assert!(empty.contains("A2A log"), "{empty}");
            assert_eq!(rig.state.thread_inspector.as_ref().unwrap().content, "");
            assert_eq!(
                rig.state.pending_log_visit,
                Some(visit),
                "empty log content paints no body row"
            );
            assert!(rig.state.log_awareness.presented.is_empty());
            rig.state.pending_log_visit = Some(visit);
            super::super::thread_open::open_log(
                &mut rig.state,
                false,
                &["M12-ATTACHED-VISIBLE".into()],
            );
            rig.state.terminal_width = 59;
            let too_small = attached_screen(&rig.paint());
            assert!(too_small.contains("Terminal too small"));
            assert_eq!(rig.state.pending_log_visit, Some(visit));
            assert!(rig.state.log_awareness.presented.is_empty());
            rig.state.terminal_width = 80;
            assert!(attached_screen(&rig.paint()).contains("M12-ATTACHED-VISIBLE"));
            assert_eq!(rig.state.log_awareness.presented, [visit]);
            assert!(rig.state.pending_log_visit.is_none());
        }
    }

    #[tokio::test]
    async fn story21_8_attached_log_closed_before_paint_does_not_commit() {
        use crate::adapters::tui::keymap::Key;
        let mut rig = attached_occlusion_rig(80, 24, false, 0);
        let visit = rig.state.pending_log_visit.unwrap();
        rig.key(Key::Esc).await;
        assert!(rig.state.modal_stack.is_empty());
        assert!(!attached_screen(&rig.paint()).contains("M12-ATTACHED-VISIBLE"));
        assert!(rig.state.log_awareness.presented.is_empty());
        super::super::thread_open::open_log(
            &mut rig.state,
            false,
            &["M12-ATTACHED-VISIBLE".into()],
        );
        rig.state.pending_log_visit = Some(visit);
        assert!(attached_screen(&rig.paint()).contains("M12-ATTACHED-VISIBLE"));
        assert_eq!(rig.state.log_awareness.presented, [visit]);
    }

    #[tokio::test]
    async fn story21_6_attached_decision_and_leader_project_the_only_row() {
        use crate::adapters::tui::keymap::Key;
        for read_only in [false, true] {
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                let mut rig = attached_occlusion_rig(width, height, read_only, 0);
                let visit = rig.state.pending_log_visit.unwrap();
                apply_attached_consolidation_frame(
                    &mut rig.state,
                    DaemonFrame::ConsolidationProposed {
                        token: crate::adapters::daemon::protocol::ProposalToken(1),
                        proposals: vec![],
                    },
                    read_only,
                );
                assert_eq!(rig.state.decisions.undecided(), usize::from(!read_only));
                if !read_only {
                    rig.ready();
                    let covered = attached_screen(&rig.paint());
                    assert!(!covered.contains("M12-ATTACHED-VISIBLE"), "{covered}");
                    assert_eq!(rig.state.pending_log_visit, Some(visit));
                    assert!(rig.state.log_awareness.presented.is_empty());
                    rig.key(Key::Esc).await;
                }
                let visible = attached_screen(&rig.paint());
                assert!(visible.contains("M12-ATTACHED-VISIBLE"), "{visible}");
                assert_eq!(rig.state.log_awareness.presented, [visit]);
                assert!(rig.frames().is_empty());
                rig.state.log_awareness.presented.clear();
                rig.state.pending_log_visit = Some(visit);
                rig.key(Key::Ctrl('x')).await;
                assert!(rig.state.pending_leader.is_some());
                rig.paint();
                assert_eq!(rig.state.pending_log_visit, Some(visit));
                assert!(rig.state.log_awareness.presented.is_empty());
                rig.key(Key::Esc).await;
                assert!(rig.state.pending_leader.is_none());
                assert!(attached_screen(&rig.paint()).contains("M12-ATTACHED-VISIBLE"));
                assert_eq!(rig.state.log_awareness.presented, [visit]);
            }
        }
    }

    #[tokio::test]
    async fn story21_8_attached_leader_still_occludes_stage_tail() {
        use crate::adapters::tui::{
            keymap::Key,
            views::{self, ViewKind},
        };
        use crate::domain::models::FeedbackLevel;
        for read_only in [false, true] {
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                let mut rig = attach_keys::tests::Rig::new(read_only);
                rig.state.terminal_width = width;
                rig.state.terminal_height = height;
                for row in 0..90 {
                    rig.domain_tab
                        .conversation
                        .messages
                        .push(system_message(format!("occlusion prefix {row}")));
                }
                views::show_view(
                    &mut rig.state.feedback_blocks,
                    ViewKind::TeamStatus,
                    "stage-occlusion",
                    FeedbackLevel::Info,
                    "M12-ATTACHED-VISIBLE",
                );
                let layout = crate::adapters::tui::layout::compute_layout(
                    ratatui::layout::Rect::new(0, 0, width, height),
                    &rig.state.theme,
                    &rig.state.input_buffer,
                    1,
                    false,
                    DensityMode::Focus,
                )
                .unwrap();
                let before = attached_screen(&rig.paint());
                assert!(
                    before
                        .lines()
                        .nth((layout.chat_pane.bottom() - 1) as usize)
                        .unwrap()
                        .contains("M12-ATTACHED-VISIBLE"),
                    "{before}"
                );
                rig.key(Key::Ctrl('x')).await;
                assert!(rig.state.pending_leader.is_some());
                let covered = rig.paint();
                let screen = attached_screen(&covered);
                assert_eq!(
                    covered.backend().buffer()
                        [(layout.chat_pane.right() - 1, layout.chat_pane.bottom() - 1)]
                        .symbol(),
                    "┘"
                );
                assert!(!screen.contains("M12-ATTACHED-VISIBLE"), "{screen}");
                assert!(rig.frames().is_empty());
            }
        }
    }

    /// M10/K11: the owning domain tab is scrolled by real ingress and the
    /// production painter consumes that exact view, not a default or a mirror.
    #[tokio::test]
    async fn story21_4_attached_scroll_is_the_shared_view_state() {
        use crate::adapters::tui::keymap::Key;
        use crate::domain::models::AnchorMode;
        use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
        for read_only in [false, true] {
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                let mut rig = attach_keys::tests::Rig::new(read_only);
                rig.state.terminal_width = width;
                rig.state.terminal_height = height;
                assert_eq!(
                    rig.state.focus,
                    FocusState::Input,
                    "every attached mode starts ready to type"
                );
                for row in 0..90 {
                    rig.domain_tab
                        .conversation
                        .messages
                        .push(system_message(format!("Bb1a-row-{row:03}")));
                }
                rig.domain_tab
                    .conversation
                    .messages
                    .push(system_message("A2b budget completion"));
                attached_frame_record(&mut rig, "initial", read_only);
                rig.type_text("/mode ").await;
                assert_eq!(rig.state.input_buffer, "/mode ");
                assert_eq!(rig.state.cursor_position, 6);
                attached_frame_record(&mut rig, "notice-command", read_only);
                rig.key(Key::Enter).await;
                assert!(rig.state.input_buffer.is_empty());
                attached_frame_record(&mut rig, "notice", read_only);
                if read_only {
                    rig.type_text("/notifications ").await;
                    assert_eq!(rig.state.input_buffer, "/notifications ");
                    assert_eq!(rig.state.cursor_position, 15);
                    attached_frame_record(&mut rig, "readonly-command", true);
                    rig.key(Key::Enter).await;
                    let log = attached_frame_record(&mut rig, "readonly-log", true);
                    assert!(log.contains("/mode"));
                } else {
                    rig.key(Key::Ctrl('x')).await;
                    rig.key(Key::Char('n')).await;
                    attached_frame_record(&mut rig, "notice-log", false);
                }
                rig.key(Key::Char('x')).await;
                rig.key(Key::Esc).await;
                assert!(rig.state.modal_stack.is_empty());
                attached_frame_record(&mut rig, "notice-dismiss-return", read_only);
                rig.type_text("C-kept draft").await;
                attached_frame_record(&mut rig, "typed", read_only);
                rig.key(Key::Ctrl('a')).await;
                assert_eq!(rig.state.cursor_position, 0);
                attached_frame_record(&mut rig, "readline-home", read_only);
                rig.key(Key::Ctrl('e')).await;
                assert_eq!(rig.state.cursor_position, 12);
                rig.key(Key::Left).await;
                assert_eq!(rig.state.cursor_position, 11);
                attached_frame_record(&mut rig, "left", read_only);
                rig.key(Key::End).await;
                rig.key(Key::Home).await;
                rig.key(Key::Right).await;
                assert_eq!(rig.state.cursor_position, 1);
                attached_frame_record(&mut rig, "caret", read_only);
                rig.paste("X").await;
                assert_eq!(rig.state.input_buffer, "CX-kept draft");
                assert_eq!(rig.state.cursor_position, 2);
                attached_frame_record(&mut rig, "paste", read_only);
                rig.key(Key::Ctrl('k')).await;
                assert_eq!(rig.state.input_buffer, "CX");
                attached_frame_record(&mut rig, "kill-tail", read_only);
                rig.key(Key::Ctrl('u')).await;
                assert!(rig.state.input_buffer.is_empty());
                attached_frame_record(&mut rig, "kill-prefix", read_only);
                rig.type_text("C-kept draft").await;
                let bottom = attached_frame_record(&mut rig, "bottom", read_only);
                assert!(bottom.contains("A2b budget completion"), "{bottom}");
                assert_eq!(rig.domain_tab.view_state.mode, AnchorMode::Following);
                rig.key(Key::PageUp).await;
                let page_offset = rig.domain_tab.view_state.scroll_offset;
                assert!(page_offset > 0, "the real key changes the owned view");
                assert_eq!(rig.domain_tab.view_state.mode, AnchorMode::Reading);
                let wheel = Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 1,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                });
                assert_eq!(
                    attach_keys::handle_attached_mouse(
                        &mut rig.state,
                        &mut rig.domain_tab.view_state,
                        &wheel
                    ),
                    None
                );
                assert!(rig.domain_tab.view_state.scroll_offset > page_offset);
                let scrolled = attached_frame_record(&mut rig, "scrolled", read_only);
                assert!(scrolled.contains("Bb1a-row-"), "{scrolled}");
                assert!(
                    !scrolled.contains("A2b budget completion"),
                    "earlier rows actually replaced the tail: {scrolled}"
                );
                let view = rig.domain_tab.view_state.clone();
                for (open, shown, returned) in
                    [('?', "help", "help-return"), ('n', "log", "log-return")]
                {
                    rig.key(Key::Ctrl('x')).await;
                    rig.key(Key::Char(open)).await;
                    let owner = rig.state.modal_stack.top().unwrap().clone();
                    assert_eq!(
                        owner.surface,
                        if open == '?' {
                            crate::adapters::tui::presentation::Surface::Help
                        } else {
                            crate::adapters::tui::presentation::Surface::NotificationLog
                        }
                    );
                    let painted = attached_frame_record(&mut rig, shown, read_only);
                    assert!(painted.contains(if open == '?' { "Help" } else { "Notifications" }));
                    assert_eq!(rig.domain_tab.view_state.scroll_offset, view.scroll_offset);
                    assert_eq!(rig.domain_tab.view_state.mode, view.mode);
                    assert_eq!(
                        attach_keys::handle_attached_mouse(
                            &mut rig.state,
                            &mut rig.domain_tab.view_state,
                            &wheel
                        ),
                        Some(crate::adapters::tui::presentation::RouteEffect::Consumed)
                    );
                    if open == '?' {
                        let before = rig.state.help_overlay.scroll_offset;
                        rig.key(Key::Char('j')).await;
                        assert_eq!(rig.state.help_overlay.scroll_offset, before + 1);
                        attached_frame_record(&mut rig, "help-scrolled", read_only);
                    }
                    rig.key(Key::Esc).await;
                    let returned_screen = attached_frame_record(&mut rig, returned, read_only);
                    let stage_height = usize::from(rig.state.viewport_height);
                    assert_eq!(
                        returned_screen
                            .lines()
                            .take(stage_height)
                            .collect::<Vec<_>>(),
                        scrolled.lines().take(stage_height).collect::<Vec<_>>(),
                        "the actual scrolled Stage returns byte-for-byte"
                    );
                    assert_eq!(rig.domain_tab.view_state.scroll_offset, view.scroll_offset);
                    assert_eq!(rig.domain_tab.view_state.mode, view.mode);
                    assert_eq!(rig.domain_tab.view_state.focused_turn, view.focused_turn);
                    assert_eq!(rig.domain_tab.view_state.collapsed, view.collapsed);
                    assert_eq!(rig.domain_tab.view_state.summary_tier, view.summary_tier);
                    assert_eq!(
                        rig.domain_tab.view_state.pending_append_lines,
                        view.pending_append_lines
                    );
                }
                assert!(
                    rig.frames().is_empty(),
                    "presentation and editing send no wire actions"
                );
            }
        }
    }

    #[tokio::test]
    async fn story21_4_attached_unicode_readline_and_paste_use_the_shared_caret() {
        use crate::adapters::tui::keymap::Key;
        for read_only in [false, true] {
            let mut rig = attach_keys::tests::Rig::new(read_only);
            rig.paste("甲e\u{301}👩‍💻 tail").await;
            rig.key(Key::Home).await;
            rig.key(Key::Right).await;
            assert_eq!(rig.state.cursor_position, 1);
            rig.key(Key::Right).await;
            assert_eq!(
                rig.state.cursor_position, 3,
                "combining grapheme is one motion"
            );
            rig.key(Key::Right).await;
            assert_eq!(rig.state.cursor_position, 6, "ZWJ grapheme is one motion");
            rig.key(Key::Left).await;
            rig.paste("界").await;
            assert_eq!(rig.state.input_buffer, "甲e\u{301}界👩‍💻 tail");
            rig.key(Key::Ctrl('k')).await;
            assert_eq!(rig.state.input_buffer, "甲e\u{301}界");
            rig.key(Key::Ctrl('h')).await;
            assert_eq!(rig.state.input_buffer, "甲e\u{301}");
            rig.key(Key::Ctrl('w')).await;
            assert!(rig.state.input_buffer.is_empty());
            rig.type_text("read only local typing").await;
            rig.key(Key::Ctrl('a')).await;
            assert_eq!(rig.state.cursor_position, 0);
            rig.key(Key::Ctrl('e')).await;
            assert_eq!(
                rig.state.cursor_position,
                rig.state.input_buffer.chars().count()
            );
            let terminal = rig.paint();
            assert!(terminal.backend().cursor_visible());
            assert!(rig.frames().is_empty());
        }
    }

    #[tokio::test]
    async fn story21_6_attached_queued_answers_reach_the_actual_wire() {
        use crate::adapters::tui::keymap::Key;
        use crate::adapters::tui::presentation::{self, Surface};
        for kind in ["consolidation", "retract"] {
            let mut rig = attach_keys::tests::Rig::new(false);
            rig.key(Key::Ctrl('p')).await;
            rig.type_text("query").await;
            let frame = rig.state.modal_stack.top().unwrap().clone();
            match kind {
                "consolidation" => apply_attached_consolidation_frame(
                    &mut rig.state,
                    DaemonFrame::ConsolidationProposed {
                        token: crate::adapters::daemon::protocol::ProposalToken(41),
                        proposals: vec![],
                    },
                    false,
                ),
                "retract" => {
                    let mut row = system_message("actual retract target");
                    row.id = "peer-response-owned-wire".into();
                    row.send_authorization = crate::domain::models::SendAuthorization::Automatic;
                    rig.domain_tab.conversation.messages.push(row);
                    use crate::adapters::tui::decision_queue::{
                        self, Responder, RetractTarget, TeamRetractPayload,
                    };
                    decision_queue::raise_team_retract(
                        &mut rig.state,
                        TeamRetractPayload {
                            target: RetractTarget::AutoResponse {
                                message_id: "peer-response-owned-wire".into(),
                            },
                            conversation_id: String::new(),
                            principal: None,
                            task: None,
                            card: "Retract actual retract target".into(),
                            armed: true,
                            request: None,
                        },
                        Responder::Attached,
                    );
                }
                _ => unreachable!(),
            }
            rig.key(Key::Char('y')).await;
            assert_eq!(rig.state.modal_stack.top(), Some(&frame));
            assert_eq!(rig.state.command_palette.filter_text.as_str(), "queryy");
            assert!(rig.frames().is_empty(), "a covered card cannot answer");
            rig.key(Key::Esc).await;
            assert!(!presentation::is_open(&rig.state, Surface::CommandPalette));
            let key = rig.state.decisions.items().next().unwrap().key();
            assert!(crate::adapters::tui::decision_queue::open(
                &mut rig.state,
                &key
            ));
            let painted = rig.paint();
            assert!(attached_screen(&painted).contains(match kind {
                "consolidation" => "memory",
                "retract" => "Retract",
                _ => unreachable!(),
            }));
            rig.ready();
            rig.key(Key::Char('y')).await;
            let frames = rig.frames();
            match (kind, frames.as_slice()) {
                (
                    "consolidation",
                    [
                        ClientFrame::ConsolidationResolve {
                            token,
                            accept: true,
                        },
                    ],
                ) => assert_eq!(token.0, 41),
                (
                    "retract",
                    [
                        ClientFrame::RetractAutoResponse {
                            message_id,
                            target_seq: None,
                        },
                    ],
                ) => assert_eq!(message_id, "peer-response-owned-wire"),
                other => {
                    panic!("state-owned answer must use its actual request identity: {other:?}")
                }
            }
            assert!(rig.state.decisions.is_empty());
            assert_eq!(rig.state.focus, FocusState::Input);
            rig.key(Key::Char('y')).await;
            assert_eq!(rig.state.input_buffer, "y");
            assert!(
                rig.frames().is_empty(),
                "settled requests never answer twice"
            );
        }
    }

    #[tokio::test]
    async fn story21_4_attached_and_local_shared_frame_scripts_keep_stack_geometry_and_status() {
        use crate::adapters::tui::{
            app,
            keymap::Key,
            presentation::{self, Shell},
        };
        use crossterm::event::Event;
        let script = [
            Key::Ctrl('p'),
            Key::Char('q'),
            Key::Ctrl('x'),
            Key::Char('?'),
            Key::Ctrl('x'),
            Key::Char('n'),
            Key::Ctrl('q'),
            Key::Char('n'),
            Key::Esc,
            Key::Esc,
            Key::Esc,
        ];
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut attached = attach_keys::tests::Rig::new(false);
            attached.state.terminal_width = width;
            attached.state.terminal_height = height;
            attached.type_text("kept draft").await;
            let mut local = TuiState::new(width, height);
            local.input_buffer = "kept draft".into();
            local.cursor_position = local.input_buffer.chars().count();
            let first = attached.paint();
            let mut local_terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut local_tabs = crate::domain::models::TabManager::default();
            let local_conversation = local_tabs.active_tab().conversation.clone();
            let local_streaming = StreamingState::default();
            let core = crate::infrastructure::runtime::agent_core::AgentCore::test_noop();
            let index = crate::domain::services::session_index::SessionIndex::new();
            let area = first.backend().buffer().area;
            let layout = crate::adapters::tui::layout::compute_layout(
                area,
                &local.theme,
                &local.input_buffer,
                1,
                false,
                DensityMode::Focus,
            )
            .unwrap();
            let status = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
                (0..width)
                    .map(|x| terminal.backend().buffer()[(x, layout.status_bar.y)].symbol())
                    .collect::<String>()
            };
            for key in script {
                let (code, modifiers) = attach_keys::tests::key_event(key);
                let raw = Event::Key(crossterm::event::KeyEvent::new(code, modifiers));
                let domain = app::convert_crossterm_event(
                    &raw,
                    &crate::domain::models::MouseConfig::default(),
                )
                .unwrap();
                let action = app::handle_operator_input(&mut local, &raw, &domain).unwrap();
                if matches!(action, app::InputAction::Quit) {
                    let mut facts = crate::adapters::tui::quit_confirm::QuitFacts::idle(
                        crate::adapters::tui::quit_confirm::QuitHost::Local,
                    );
                    facts.composer_draft = !local.input_buffer.is_empty();
                    presentation::reducer::request_quit(&mut local, facts);
                }
                attached.key(key).await;
                let local_stack: Vec<_> = local
                    .modal_stack
                    .iter()
                    .map(|f| (f.id, f.surface))
                    .collect();
                let attached_stack: Vec<_> = attached
                    .state
                    .modal_stack
                    .iter()
                    .map(|f| (f.id, f.surface))
                    .collect();
                assert_eq!(attached_stack, local_stack, "{key:?}");
                assert_eq!(attached.state.pending_leader, local.pending_leader);
                assert_eq!(attached.state.focus, local.focus);
                assert_eq!(attached.state.input_buffer, local.input_buffer);
                crate::infrastructure::runtime::event_loop::render(
                    &mut local_terminal,
                    &mut local,
                    &local_conversation,
                    &local_streaming,
                    "fixture-model",
                    "fixture-provider",
                    &crate::adapters::noop::NoOpSecurity,
                    1,
                    0,
                    Some(&mut local_tabs),
                    &index,
                    128_000,
                    &core,
                )
                .unwrap();
                let rendered = attached.paint();
                assert!(
                    status(&local_terminal).contains("Ready"),
                    "actual local status remains outside frames"
                );
                assert_eq!(
                    local_terminal.backend().cursor_visible(),
                    rendered.backend().cursor_visible()
                );
                let attached_layout = crate::adapters::tui::layout::compute_layout(
                    area,
                    &attached.state.theme,
                    &attached.state.input_buffer,
                    1,
                    false,
                    DensityMode::Focus,
                )
                .unwrap();
                let local_shell = Shell::new(area, &layout);
                let attached_shell = Shell::new(area, &attached_layout);
                assert_eq!(local_shell.region, attached_shell.region);
                assert_eq!(local_shell.status_bar, attached_shell.status_bar);
                let rect = presentation::geometry::frame_rect(attached_shell.region);
                if !attached_stack.is_empty() && attached.state.pending_leader.is_none() {
                    assert_eq!(rendered.backend().buffer()[(rect.x, rect.y)].symbol(), "┌");
                    assert_eq!(
                        rendered.backend().buffer()[(rect.right() - 1, rect.bottom() - 1)].symbol(),
                        "┘"
                    );
                }
                assert!(status(&rendered).contains("Ready"));
                assert_eq!(attached_shell.status_bar, layout.status_bar);
                assert!(attached.frames().is_empty());
            }
            assert!(attached.state.modal_stack.is_empty() && local.modal_stack.is_empty());
            assert_eq!(status(&attached.paint()), status(&first));
        }
    }

    #[test]
    fn story21_4_attached_production_paint_uses_shared_minimum_size() {
        for read_only in [false, true] {
            for (width, height) in [(59, 24), (80, 15)] {
                let mut rig = attach_keys::tests::Rig::new(read_only);
                rig.state.terminal_width = width;
                rig.state.terminal_height = height;
                let terminal = rig.paint();
                let screen = attached_screen(&terminal);
                assert!(screen.contains("Terminal too small (min 60x16)"));
                assert!(!screen.contains("Message") && !screen.contains("Ready"));
                assert!(!terminal.backend().cursor_visible());
                assert_eq!(rig.state.focus, FocusState::Input);
            }
        }
    }

    /// Story 21.13 (D4/M9): the code cap is a local-Stage setting; the attached
    /// host paints every code row whatever the local value is.
    #[test]
    fn story21_13_attached_code_is_never_capped() {
        use crate::domain::models::{Turn, TurnPart};
        let mut rig = attach_keys::tests::Rig::new(false);
        rig.state.terminal_width = 80;
        rig.state.terminal_height = 70;
        rig.state.code_max_rows = 3;
        let body = (1..=45)
            .map(|n| format!("fn line_{n:02}() {{}}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut turn = Turn::new("model".into(), 0);
        turn.push_part(|id| TurnPart::Prose {
            id,
            text: format!("```rust\n{body}\n```\n"),
        });
        rig.domain_tab.conversation.turns.push(turn);
        rig.domain_tab.conversation.rebuild_messages_mirror();
        let screen = attached_screen(&rig.paint());
        assert!(screen.contains("fn line_01()"), "{screen}");
        assert!(screen.contains("fn line_45()"), "{screen}");
        assert!(!screen.contains("more line"), "{screen}");
    }

    /// Moved from conformance_18_3d_consent: labels and dispatch are observed
    /// through the real producer, attached state, painter, front door and wire.
    #[tokio::test]
    async fn consent_card_labels_have_guarded_non_test_dispatch_arms() {
        use crate::adapters::tui::keymap::Key;
        use crate::domain::models::tool_call::{ApprovalSource, RequestId};
        use crate::domain::models::{ApprovalOutcome, ApprovalScope, PolicyPrincipal, ToolRisk};
        let sender = PolicyPrincipal::Pseudonymous(
            crate::domain::models::PeerId::from_public_key(&[71; 32]).unwrap(),
        );
        let summary =
            crate::infrastructure::runtime::transparency_bridge::consent_card_text(&sender);
        for label in ["[y] Allow once", "[a] Always allow", "[n] Decline"] {
            assert!(
                summary.contains(label),
                "actual producer must render its action: {summary}"
            );
        }
        for (key, expected) in [
            ('y', ApprovalOutcome::Once),
            (
                'a',
                ApprovalOutcome::AlwaysAndSave {
                    scope: ApprovalScope::Tool("a2a/sender-consent".into()),
                },
            ),
            ('n', ApprovalOutcome::Reject { feedback: None }),
        ] {
            let mut rig = attach_keys::tests::Rig::new(false);
            rig.state.input_buffer = "draft".into();
            rig.state.cursor_position = 5;
            rig.key(Key::Ctrl('x')).await;
            rig.key(Key::Char('?')).await;
            let owner = rig.state.modal_stack.top().unwrap().clone();
            rig.approval_frame(DaemonFrame::ApprovalRequest {
                request_id: RequestId("real-consent-dispatch".into()),
                tool: "a2a/sender-consent".into(),
                source: ApprovalSource::ForegroundTurn {
                    conversation_id: rig.domain_tab.conversation.id.clone(),
                },
                risk: ToolRisk::Standard,
                summary: summary.clone(),
                input_bytes: None,
            });
            rig.key(Key::Char(key)).await;
            assert_eq!(rig.state.modal_stack.top(), Some(&owner));
            assert_eq!(rig.state.decisions.undecided(), 1);
            assert!(rig.frames().is_empty(), "covered consent owns no keys");
            rig.key(Key::Esc).await;
            rig.key(Key::Ctrl('x')).await;
            rig.key(Key::Char('q')).await;
            rig.key(Key::Enter).await;
            rig.ready();
            let screen = attached_screen(&rig.paint());
            for label in ["Allow once", "Always allow", "Decline"] {
                assert!(
                    screen.contains(label),
                    "actual attached card is visible: {screen}"
                );
            }
            rig.key(Key::Char(key)).await;
            assert!(
                matches!(rig.frames().as_slice(), [ClientFrame::ApprovalResponse { request_id, outcome }]
                if request_id.0 == "real-consent-dispatch" && outcome == &expected)
            );
            assert!(rig.state.decisions.is_empty());
            assert_eq!(rig.state.focus, FocusState::Input);
            rig.key(Key::Char(key)).await;
            assert_eq!(rig.state.input_buffer, format!("draft{key}"));
            assert!(
                rig.frames().is_empty(),
                "an empty consent queue never answers"
            );
        }
        let mut reader = attach_keys::tests::Rig::new(true);
        reader.approval_frame(DaemonFrame::ApprovalRequest {
            request_id: RequestId("reader-consent".into()),
            tool: "a2a/sender-consent".into(),
            source: ApprovalSource::ForegroundTurn {
                conversation_id: reader.domain_tab.conversation.id.clone(),
            },
            risk: ToolRisk::Standard,
            summary,
            input_bytes: None,
        });
        reader.key(Key::Char('y')).await;
        assert_eq!(reader.state.focus, FocusState::Input);
        assert_eq!(reader.state.input_buffer, "y");
        assert!(reader.state.decisions.is_empty());
        assert!(
            reader.frames().is_empty(),
            "read-only arrival cannot take the composer's typing"
        );
    }

    // Supporting seams; signed socket/PTY K02 evidence is run after integration.
    mod exact_attached_approval {
        use super::*;
        use crate::adapters::daemon::protocol::ApprovalRefusal;
        use crate::adapters::tui::decision_queue::{DecisionSubject, PermissionInput};
        use crate::domain::models::ToolRisk;
        use crate::domain::models::tool_call::{ApprovalSource, RequestId};

        fn request(id: &str, data: &str) -> DaemonFrame {
            DaemonFrame::ApprovalRequest {
                request_id: RequestId(id.into()),
                tool: "Bash".into(),
                source: ApprovalSource::ForegroundTurn {
                    conversation_id: "c".into(),
                },
                risk: ToolRisk::Standard,
                summary: "command summary".into(),
                input_bytes: Some(data.len() as u64),
            }
        }

        fn input(state: &TuiState) -> &PermissionInput {
            let DecisionSubject::Permission(payload) =
                &state.decisions.items().next().unwrap().subject
            else {
                panic!("permission payload");
            };
            &payload.input
        }

        fn chunk(id: &str, sequence: u32, data: &str) -> DaemonFrame {
            DaemonFrame::ApprovalInputChunk {
                request_id: RequestId(id.into()),
                sequence,
                data: data.into(),
            }
        }

        fn complete(id: &str, chunks: u32) -> DaemonFrame {
            DaemonFrame::ApprovalInputComplete {
                request_id: RequestId(id.into()),
                chunks,
            }
        }

        #[test]
        fn rich_exact_receiving_until_valid_complete() {
            let mut state = TuiState::new(80, 24);
            let mut assemblies = HashMap::new();
            let data = r#"{"command":"unique exact tail"}"#;
            apply_attached_approval_frame(&mut state, &mut assemblies, request("a", data), false);
            assert!(matches!(input(&state), PermissionInput::Receiving { .. }));
            apply_attached_approval_frame(&mut state, &mut assemblies, chunk("a", 0, data), false);
            assert!(matches!(input(&state), PermissionInput::Receiving { .. }));
            apply_attached_approval_frame(&mut state, &mut assemblies, complete("a", 1), false);
            assert!(matches!(input(&state), PermissionInput::Ready(value)
                if **value == serde_json::json!({"command": "unique exact tail"})));
            assert!(assemblies.is_empty());
        }

        #[test]
        fn rich_invalid_stale_and_other_request_transfers_never_publish() {
            let data = r#"{"command":"late tail"}"#;
            for fault in [
                "partial",
                "duplicate",
                "out-of-order",
                "overflow",
                "wrong-request",
                "wrong-count",
            ] {
                let mut state = TuiState::new(80, 24);
                let mut assemblies = HashMap::new();
                apply_attached_approval_frame(
                    &mut state,
                    &mut assemblies,
                    request("a", data),
                    false,
                );
                let parts = match fault {
                    "partial" => vec![chunk("a", 0, &data[..5])],
                    "duplicate" => vec![chunk("a", 0, data), chunk("a", 0, data)],
                    "out-of-order" => vec![chunk("a", 1, data), chunk("a", 0, data)],
                    "overflow" => vec![chunk("a", 0, &format!("{data}x"))],
                    "wrong-request" => vec![chunk("b", 0, data)],
                    _ => vec![chunk("a", 0, data)],
                };
                for part in parts {
                    apply_attached_approval_frame(&mut state, &mut assemblies, part, false);
                }
                apply_attached_approval_frame(
                    &mut state,
                    &mut assemblies,
                    complete("a", if fault == "wrong-count" { 2 } else { 1 }),
                    false,
                );
                assert!(
                    matches!(input(&state), PermissionInput::Receiving { .. }),
                    "{fault}"
                );
            }
        }

        #[test]
        fn duplicate_and_refusal_lifecycle() {
            let mut state = TuiState::new(80, 24);
            state.input_buffer = "draft".into();
            let mut assemblies = HashMap::new();
            let data = r#"{"command":"ok"}"#;
            apply_attached_approval_frame(&mut state, &mut assemblies, request("a", data), false);
            apply_attached_approval_frame(&mut state, &mut assemblies, chunk("a", 0, data), false);
            apply_attached_approval_frame(&mut state, &mut assemblies, request("a", data), false);
            apply_attached_approval_frame(&mut state, &mut assemblies, complete("a", 1), false);
            assert_eq!(state.decisions.undecided(), 1);
            assert!(matches!(input(&state), PermissionInput::Ready(_)));
            for reason in [
                ApprovalRefusal::WriterOnly,
                ApprovalRefusal::IssuedElsewhere,
            ] {
                apply_attached_approval_frame(
                    &mut state,
                    &mut assemblies,
                    DaemonFrame::ApprovalRefused {
                        request_id: RequestId("a".into()),
                        reason,
                    },
                    false,
                );
                assert_eq!(state.decisions.undecided(), 1);
                assert!(
                    state
                        .notifications
                        .records()
                        .any(|notice| notice.source == NoticeSource::Approval
                            && notice.message == reason.to_string())
                );
            }
            apply_attached_approval_frame(
                &mut state,
                &mut assemblies,
                DaemonFrame::ApprovalRefused {
                    request_id: RequestId("a".into()),
                    reason: ApprovalRefusal::NoLongerPending,
                },
                false,
            );
            assert!(state.decisions.is_empty());
        }

        #[test]
        fn read_only_never_admits_private_input() {
            let mut state = TuiState::new(80, 24);
            let mut assemblies = HashMap::new();
            let data = r#"{"command":"private"}"#;
            for frame in [request("a", data), chunk("a", 0, data), complete("a", 1)] {
                apply_attached_approval_frame(&mut state, &mut assemblies, frame, true);
            }
            assert!(state.decisions.is_empty());
            assert!(assemblies.is_empty());
        }
    }

    /// In-crate nearest-production-path seams, not native socket/PTY evidence.
    mod attached_proposals {
        use super::*;
        use crate::adapters::daemon::protocol::TeamProposalCloseOutcome;
        use crate::adapters::tui::decision_queue::{self, DecisionKind, DecisionSubject};
        use crate::domain::models::{SemanticMessageType, TeamProposalDecision};
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

        type Rig = attach_keys::tests::Rig;

        fn frame(rig: &mut Rig, frame: DaemonFrame, read_only: bool) {
            let conversation = rig.domain_tab.conversation.id.clone();
            apply_attached_proposal_frame(
                &mut rig.state,
                read_only,
                &mut rig.team_requests,
                &conversation,
                frame,
            );
        }

        fn proposal(id: &str, body: &str) -> DaemonFrame {
            DaemonFrame::TeamProposal {
                proposal: id.into(),
                recipients: vec!["lena-po".into(), "jun-dev".into()],
                message_type: SemanticMessageType::ArchitectureUpdate,
                body: body.into(),
            }
        }

        fn rig(body: &str) -> Rig {
            let mut rig = Rig::new(false);
            rig.paint();
            frame(&mut rig, proposal("tp_attached", body), false);
            rig.ready();
            rig
        }

        fn item(rig: &Rig) -> &decision_queue::DecisionItem {
            rig.state
                .decisions
                .items()
                .find(|item| item.kind == DecisionKind::TeamProposal)
                .unwrap()
        }

        fn snapshot(rig: &Rig) -> Arc<crate::domain::models::TeamProposal> {
            let DecisionSubject::TeamProposal(payload) = &item(rig).subject else {
                panic!("proposal payload")
            };
            Arc::clone(&payload.proposal)
        }

        async fn key(rig: &mut Rig, code: KeyCode) {
            rig.raw_key(KeyEvent::new(code, KeyModifiers::NONE)).await;
        }

        async fn send(rig: &mut Rig) {
            key(rig, KeyCode::Char('y')).await;
            let frames = rig.frames();
            assert!(
                matches!(frames.as_slice(), [ClientFrame::TeamProposalDecision {
                proposal, decision: TeamProposalDecision::Send
            }] if proposal == "tp_attached"),
                "{frames:?}"
            );
        }

        fn screen(rig: &mut Rig) -> String {
            let terminal = rig.paint();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_enter_escape_defer_without_send() {
            let mut rig = rig("body");
            key(&mut rig, KeyCode::Enter).await;
            assert!(rig.frames().is_empty());
            key(&mut rig, KeyCode::Esc).await;
            assert!(rig.frames().is_empty());
            assert!(item(&rig).deferred);
            let decision = item(&rig).key();
            assert!(decision_queue::open(&mut rig.state, &decision));
            rig.ready();
            send(&mut rig).await;
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_draft_gate_and_pending_reply() {
            let mut rig = rig("proposal body");
            let original = snapshot(&rig);
            rig.state.input_buffer = "draft yrn".into();
            rig.state.cursor_position = rig.state.input_buffer.chars().count();
            let text = screen(&mut rig);
            assert!(
                text.contains("Finish or clear your draft to decide."),
                "{text}"
            );
            assert!(!text.contains("[y] Send to"), "{text}");
            for c in ['y', 'r', 'n'] {
                key(&mut rig, KeyCode::Char(c)).await;
                assert!(rig.frames().is_empty());
                assert!(Arc::ptr_eq(&original, &snapshot(&rig)));
            }
            assert_eq!(rig.state.input_buffer, "draft yrn");
            rig.state.input_buffer = "/team reply ri_pending operator draft".into();
            rig.state.cursor_position = rig.state.input_buffer.chars().count();
            assert!(matches!(
                attached_composer_key_with_team_requests(
                    KeyCode::Enter,
                    false,
                    &mut rig.state,
                    &rig.tx,
                    &mut rig.team_requests,
                ),
                ComposerOutcome::FrameSent
            ));
            assert!(
                matches!(rig.frames().as_slice(), [ClientFrame::ReplyToRecipientItem { item_id, text, .. }]
                if item_id == "ri_pending" && text == "operator draft")
            );
            assert!(screen(&mut rig).contains("Finish or clear your draft to decide."));
            key(&mut rig, KeyCode::Char('y')).await;
            assert!(rig.frames().is_empty());
            assert!(Arc::ptr_eq(&original, &snapshot(&rig)));
            use crate::adapters::daemon::protocol::{
                RecipientAction, RecipientActionOutcome, RecipientActionTarget,
            };
            crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                &mut rig.state,
                RecipientAction::Reply,
                RecipientActionTarget::Item {
                    item_id: "ri_pending".into(),
                },
                RecipientActionOutcome::Applied { reply: None },
            );
            rig.ready();
            send(&mut rig).await;
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_review_uncapped_delimited_in_frame() {
            let body = format!(
                "{}\nTo: x\n[y] Send to x",
                (0..45)
                    .map(|i| format!("body-row-{i:02} e\u{301}界"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let mut rig = rig(&body);
            key(&mut rig, KeyCode::Char('r')).await;
            assert!(rig.frames().is_empty());
            assert!(matches!(&item(&rig).subject, DecisionSubject::TeamProposal(p) if p.reviewing));
            for width in [56, 76, 126] {
                let lines = crate::adapters::tui::widgets::decision_frame::body_lines_for_test(
                    item(&rig),
                    &rig.state,
                    width,
                );
                let rows: Vec<String> = lines
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect()
                    })
                    .collect();
                assert!(rows.len() > 45);
                assert!(rows.iter().any(|row| row == "│ body-row-44 e\u{301}界"));
                assert!(rows.iter().any(|row| row == "│ To: x"));
                assert!(rows.iter().any(|row| row == "│ [y] Send to x"));
                let start = rows.iter().position(|row| row.starts_with("│ ")).unwrap();
                assert!(rows[start..].iter().all(|row| row.starts_with("│ ")));
            }
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                rig.state.terminal_width = width;
                rig.state.terminal_height = height;
                let painted = screen(&mut rig);
                assert!(painted.contains("Decision:"), "{painted}");
                assert!(rig.frames().is_empty());
            }
            key(&mut rig, KeyCode::Esc).await;
            rig.ready();
            send(&mut rig).await;
        }

        #[tokio::test]
        async fn story21_6_attached_review_restores_received_surface_and_transcript_position() {
            use crate::adapters::tui::state::ThreadAccess;
            let workspace = tempfile::tempdir().unwrap();
            let journal =
                crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
                    workspace.path(),
                )
                .await
                .unwrap();
            let address = append_received_test_item(
                &journal,
                "ri_review_overlay",
                SemanticMessageType::ArchitectureUpdate,
                "RECEIVED_ITEM_OVERLAY_BODY",
            )
            .await;
            let mut surface = rig("CURRENT_PROPOSAL_FIRST\nCURRENT_PROPOSAL_LAST");
            key(&mut surface, KeyCode::Esc).await;
            surface.domain_tab.view_state.scroll_offset = 100;
            surface.domain_tab.view_state.mode = crate::domain::models::AnchorMode::Reading;
            crate::infrastructure::runtime::thread_open::open_received(
                workspace.path(),
                None,
                address.item().as_str(),
                &mut surface.state,
                ThreadAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            let before = surface.domain_tab.view_state.scroll_offset;
            let decision = item(&surface).key();
            assert!(decision_queue::open(&mut surface.state, &decision));
            surface.ready();
            key(&mut surface, KeyCode::Char('r')).await;
            let rows = crate::adapters::tui::widgets::decision_frame::body_lines_for_test(
                item(&surface),
                &surface.state,
                76,
            );
            let rows: Vec<String> = rows
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect();
            assert!(rows.iter().any(|row| row == "│ CURRENT_PROPOSAL_FIRST"));
            assert!(rows.iter().any(|row| row == "│ CURRENT_PROPOSAL_LAST"));
            assert!(surface.frames().is_empty());
            key(&mut surface, KeyCode::Esc).await;
            key(&mut surface, KeyCode::Esc).await;
            assert!(surface.state.thread_inspector.is_some());
            assert_eq!(surface.domain_tab.view_state.scroll_offset, before);
            assert!(surface.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_dismiss_and_send_receipts() {
            let mut dismissed = rig("private body");
            key(&mut dismissed, KeyCode::Char('n')).await;
            assert!(
                matches!(dismissed.frames().as_slice(), [ClientFrame::TeamProposalDecision {
                proposal, decision: TeamProposalDecision::Dismiss
            }] if proposal == "tp_attached")
            );
            assert!(dismissed.team_requests.pending.is_empty());
            assert!(dismissed.state.team_send_blocks.is_empty());
            assert_eq!(
                dismissed.state.feedback_blocks["team-proposal-status"].message,
                "Proposal dismissed. Nothing was sent."
            );
            let mut sent = rig("exact\n\tbody e\u{301}界");
            let original = snapshot(&sent);
            send(&mut sent).await;
            assert_eq!(original.body, "exact\n\tbody e\u{301}界");
            assert!(sent.team_requests.pending.contains_key("tp_attached"));
            assert!(!decision_queue::team_proposal_pending(&sent.state));
            let mut typed = TuiState::new(80, 24);
            let mut requests = AttachedTeamRequests::default();
            let (tx, mut rx) = mpsc::unbounded_channel();
            assert!(enqueue_attached_team_frame(
                &mut typed,
                &mut requests,
                ClientFrame::TeamSend {
                    request: "typed".into(),
                    recipients: original.recipients.clone(),
                    text: original.body.clone(),
                    message_type: Some(original.message_type),
                    subject: None,
                    stories: None,
                    holders_of: None,
                },
                &tx
            ));
            assert!(matches!(
                rx.try_recv().unwrap(),
                ClientFrame::TeamSend { .. }
            ));
            let block = match &sent.team_requests.pending["tp_attached"] {
                AttachedTeamRequest::Send {
                    block_id,
                    action: None,
                    ..
                } => block_id.clone(),
                other => panic!("ordinary send block expected: {other:?}"),
            };
            assert_eq!(
                sent.state.feedback_blocks[&block].message,
                typed.feedback_blocks["team-send-1"].message
            );
            for (state, requests, request) in [
                (&mut sent.state, &mut sent.team_requests, "tp_attached"),
                (&mut typed, &mut requests, "typed"),
            ] {
                assert!(apply_attached_team_frame(
                    state,
                    requests,
                    DaemonFrame::TeamSendStarted {
                        request: request.into(),
                        action: crate::domain::models::SendActionId::mint(),
                        recipients: Vec::new(),
                    },
                    false,
                    &tx
                ));
                for index in 0..2 {
                    assert!(apply_attached_team_frame(
                        state,
                        requests,
                        DaemonFrame::TeamSendSettled {
                            request: request.into(),
                            index,
                            outcome: crate::adapters::daemon::protocol::TeamSendOutcome::Delivered,
                        },
                        false,
                        &tx
                    ));
                }
            }
            assert_eq!(
                sent.state.feedback_blocks[&block].message,
                typed.feedback_blocks["team-send-1"].message
            );
            assert!(sent.team_requests.pending.is_empty());
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_busy_refusal_and_withdrawal_are_correlated() {
            let mut rig = rig("body");
            let original = snapshot(&rig);
            let busy = "Not sent yet — this daemon is busy sending. The proposal is still waiting.";
            frame(
                &mut rig,
                DaemonFrame::TeamProposalClosed {
                    proposal: "tp_attached".into(),
                    outcome: TeamProposalCloseOutcome::Refused {
                        reason: busy.into(),
                    },
                },
                false,
            );
            assert!(Arc::ptr_eq(&original, &snapshot(&rig)));
            assert_eq!(
                rig.state.feedback_blocks["team-proposal-status"].message,
                busy
            );
            frame(
                &mut rig,
                DaemonFrame::TeamProposalClosed {
                    proposal: "unrelated".into(),
                    outcome: TeamProposalCloseOutcome::Withdrawn,
                },
                false,
            );
            assert!(Arc::ptr_eq(&original, &snapshot(&rig)));
            rig.ready();
            send(&mut rig).await;
            frame(
                &mut rig,
                DaemonFrame::TeamProposalClosed {
                    proposal: "tp_attached".into(),
                    outcome: TeamProposalCloseOutcome::Refused {
                        reason: busy.into(),
                    },
                },
                false,
            );
            assert!(
                !decision_queue::team_proposal_pending(&rig.state),
                "refusal cannot resurrect a settled item"
            );
            frame(&mut rig, proposal("tp_attached", "body"), false);
            frame(
                &mut rig,
                DaemonFrame::TeamProposalClosed {
                    proposal: "tp_attached".into(),
                    outcome: TeamProposalCloseOutcome::Withdrawn,
                },
                false,
            );
            assert!(!decision_queue::team_proposal_pending(&rig.state));
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_6_attached_open_withdrawal_is_scoped_and_cannot_send() {
            let mut rig = rig("body");
            frame(&mut rig, proposal("other", "other body"), false);
            frame(
                &mut rig,
                DaemonFrame::TeamProposalClosed {
                    proposal: "tp_attached".into(),
                    outcome: TeamProposalCloseOutcome::Withdrawn,
                },
                false,
            );
            assert!(matches!(
                item(&rig).validity,
                decision_queue::Validity::Stale(decision_queue::PROPOSAL_WITHDRAWN)
            ));
            let text = screen(&mut rig);
            assert!(
                text.contains("Proposal withdrawn with the conversation."),
                "{text}"
            );
            assert!(
                rig.state
                    .decisions
                    .items()
                    .any(|item| item.source == "other" && !item.is_stale())
            );
            key(&mut rig, KeyCode::Char('y')).await;
            assert!(rig.frames().is_empty());
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_conversation_and_failed_enqueue_keep_snapshot() {
            let mut surface = rig("exact body");
            let original = snapshot(&surface);
            let conversation = surface.domain_tab.conversation.id.clone();
            surface.domain_tab.conversation.id = "other-conversation".into();
            surface.paint();
            key(&mut surface, KeyCode::Char('y')).await;
            assert!(surface.frames().is_empty());
            assert!(Arc::ptr_eq(&original, &snapshot(&surface)));
            // A key of a frame that is not decidable here reaches the owner beneath (the
            // composer); the T9 gate would then rightly block the next decision.
            assert_eq!(surface.state.input_buffer, "y");
            surface.state.input_buffer.clear();
            surface.state.cursor_position = 0;
            surface.domain_tab.conversation.id = conversation;
            surface.paint();
            let decision = item(&surface).key();
            assert!(decision_queue::open(&mut surface.state, &decision));
            surface.ready();
            surface.close_receiver();
            key(&mut surface, KeyCode::Char('y')).await;
            assert!(Arc::ptr_eq(&original, &snapshot(&surface)));
            assert!(surface.team_requests.pending.is_empty());
            assert_eq!(
                surface.state.feedback_blocks["team-proposal-status"].message,
                "Attach socket closed — the proposal decision was not sent."
            );
            let mut positive = rig("exact body");
            send(&mut positive).await;
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_waits_behind_consolidation_and_can_open_after_defer() {
            let mut surface = Rig::new(false);
            surface.paint();
            decision_queue::raise_attached_consolidation(
                &mut surface.state,
                crate::adapters::daemon::protocol::ProposalToken(1),
                Vec::new(),
            );
            surface.ready();
            frame(&mut surface, proposal("tp_attached", "body"), false);
            assert!(decision_queue::team_proposal_pending(&surface.state));
            assert!(surface.frames().is_empty());
            key(&mut surface, KeyCode::Esc).await;
            assert!(surface.frames().is_empty());
            assert!(surface.state.decisions.holds(DecisionKind::Consolidation));
            let proposal_key = item(&surface).key();
            assert!(decision_queue::open(&mut surface.state, &proposal_key));
            surface.ready();
            send(&mut surface).await;
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_does_not_steal_other_decision_keys() {
            use decision_queue::{
                ConsentPayload, PermissionInput, PermissionPayload, Responder, RetractTarget,
                TeamRetractPayload,
            };
            for other in ["permission", "consent", "retract"] {
                let mut surface = Rig::new(false);
                surface.paint();
                let request = crate::domain::models::RequestId::new();
                match other {
                    "permission" => {
                        let conversation = surface.domain_tab.conversation.id.clone();
                        decision_queue::raise_permission(
                            &mut surface.state,
                            PermissionPayload::new(
                                request.clone(),
                                crate::domain::models::ApprovalSource::ForegroundTurn {
                                    conversation_id: conversation,
                                },
                                "Write".into(),
                                crate::domain::models::ToolRisk::Safe,
                                PermissionInput::Ready(Arc::new(serde_json::json!({}))),
                            ),
                            Responder::Attached,
                        );
                    }
                    "consent" => {
                        decision_queue::raise_consent(
                            &mut surface.state,
                            ConsentPayload {
                                request_id: request,
                                tool: "a2a/sender-consent".into(),
                                input_preview: "consent".into(),
                            },
                        );
                    }
                    _ => {
                        decision_queue::raise_team_retract(
                            &mut surface.state,
                            TeamRetractPayload {
                                target: RetractTarget::AutoResponse {
                                    message_id: "m1".into(),
                                },
                                conversation_id: String::new(),
                                principal: None,
                                task: None,
                                card: "retract".into(),
                                armed: true,
                                request: None,
                            },
                            Responder::Attached,
                        );
                    }
                }
                surface.ready();
                frame(&mut surface, proposal("tp_attached", "body"), false);
                key(&mut surface, KeyCode::Char('y')).await;
                assert!(
                    surface
                        .frames()
                        .iter()
                        .all(|frame| !matches!(frame, ClientFrame::TeamProposalDecision { .. })),
                    "{other} must own the active decision key"
                );
                assert!(decision_queue::team_proposal_pending(&surface.state));
            }
            let mut positive = rig("body");
            send(&mut positive).await;
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_preview_aliases_type_and_spoof_delimiters() {
            let mut surface = rig("To: x\n[y] Send to x\nthird\nfourth\nfifth");
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                surface.state.terminal_width = width;
                surface.state.terminal_height = height;
                let rows = crate::adapters::tui::widgets::decision_frame::body_lines_for_test(
                    item(&surface),
                    &surface.state,
                    usize::from(width - 4),
                );
                let rows: Vec<String> = rows
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect()
                    })
                    .collect();
                assert!(
                    rows.iter()
                        .any(|row| row.contains("lena-po") && row.contains("jun-dev"))
                );
                assert!(rows.iter().any(|row| row.contains("architecture_update")));
                assert!(rows.iter().any(|row| row == "│ To: x"));
                assert!(rows.iter().any(|row| row == "│ [y] Send to x"));
                assert!(rows.iter().any(|row| row.contains("more not shown")));
                let painted = screen(&mut surface);
                assert!(
                    painted.contains("Send to")
                        && painted.contains("Review first")
                        && painted.contains("Dismiss"),
                    "{painted}"
                );
                assert!(!painted.contains("Delivered"));
            }
            send(&mut surface).await;
        }

        #[tokio::test]
        async fn story21_6_attached_proposal_raw_keys_and_read_only_do_not_decide() {
            let mut mirror = Rig::new(true);
            mirror.paint();
            frame(&mut mirror, proposal("tp_attached", "private body"), true);
            assert!(!decision_queue::team_proposal_pending(&mirror.state));
            assert!(mirror.frames().is_empty());
            for kind in [
                KeyEventKind::Press,
                KeyEventKind::Repeat,
                KeyEventKind::Release,
            ] {
                for modifiers in [
                    KeyModifiers::NONE,
                    KeyModifiers::SHIFT,
                    KeyModifiers::ALT,
                    KeyModifiers::CONTROL,
                ] {
                    if kind == KeyEventKind::Press && modifiers == KeyModifiers::NONE {
                        continue;
                    }
                    for c in ['y', 'r', 'n'] {
                        let mut surface = rig("body");
                        surface
                            .raw_key(KeyEvent::new_with_kind(KeyCode::Char(c), modifiers, kind))
                            .await;
                        assert!(surface.frames().is_empty(), "{kind:?} {modifiers:?} {c}");
                        assert!(decision_queue::team_proposal_pending(&surface.state));
                    }
                }
            }
            let mut positive = rig("body");
            send(&mut positive).await;
        }
    }

    fn consent_frame(id: &str, summary: String) -> DaemonFrame {
        DaemonFrame::ApprovalRequest {
            request_id: crate::domain::models::RequestId(id.into()),
            tool: "a2a/sender-consent".into(),
            source: crate::domain::models::ApprovalSource::ForegroundTurn {
                conversation_id: "c".into(),
            },
            risk: crate::domain::models::ToolRisk::Standard,
            summary,
            input_bytes: None,
        }
    }

    #[test]
    fn consent_queue_indicator_is_derived_from_the_session_queue() {
        let mut rig = attach_keys::tests::Rig::new(false);
        rig.state.input_buffer = "draft".into();
        for id in ["a", "b", "c"] {
            rig.approval_frame(consent_frame(id, "Consent required".into()));
        }
        assert_eq!(attached_notice_badges(&rig.state).decisions, 3);
        let text = attached_screen(&rig.paint());
        assert!(text.contains("Needs you (3) ^X q"), "{text}");
    }

    #[test]
    fn an_overlong_consent_line_wraps_inside_the_frame_at_the_layout_floor() {
        use crate::domain::services::peer_admission::BOUND_KEY_CONSENT_LINE;
        let pin = crate::domain::models::PeerId::from_public_key(&[9; 32]).unwrap();
        let summary = crate::infrastructure::runtime::transparency_bridge::consent_card_text(
            &crate::domain::models::PolicyPrincipal::bound_a2a(pin),
        );
        let mut rig = attach_keys::tests::Rig::new(false);
        rig.state.terminal_width = 60;
        rig.state.terminal_height = 30;
        rig.approval_frame(consent_frame("bound", summary));
        let text = attached_screen(&rig.paint());
        let words = |s: &str| {
            s.replace('│', " ")
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let rendered = words(&text);
        let clause = words(BOUND_KEY_CONSENT_LINE);
        assert!(
            rendered
                .windows(clause.len())
                .any(|window| window == clause.as_slice()),
            "{text}"
        );
        assert!(text.contains("[y] Allow once"), "{text}");
        assert!(text.contains("[a] Always allow"), "{text}");
    }

    // ── Story 18.3c (AC3/AC5) — peer response decision-core coverage ────────

    fn peer_row(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            id: id.to_owned(),
            role: MessageRole::Assistant,
            content: content.to_owned(),
            ..Default::default()
        }
    }

    fn peer_control(
        node: &str,
        stage: crate::domain::models::PeerRowStage,
    ) -> crate::domain::models::PeerRowControl {
        crate::domain::models::PeerRowControl {
            node_id: node.to_owned(),
            item_address: None,
            stories: None,
            status: None,
            selected_for: None,
            retro: None,
            message_type: crate::domain::models::SemanticMessageType::Consultation,
            sender_label: "sender".to_owned(),
            stage,
            node_resolvable: true,
        }
    }

    fn recipient_body(
        conversation: &Conversation,
        state: &mut TuiState,
        width: usize,
    ) -> Vec<String> {
        use crate::adapters::tui::{
            decision_queue::{self, DecisionKind},
            widgets::decision_frame,
        };
        decision_queue::sync_recipient_drafts(state, conversation, false);
        let item = state
            .decisions
            .items()
            .find(|item| item.kind == DecisionKind::RecipientDraftApproval && !item.is_stale())
            .expect("recipient decision");
        decision_frame::body_lines_for_test(item, state, width)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn story21_6_attached_recipient_body_preserves_alias_and_principal_without_controls() {
        use crate::domain::models::{ItemAddress, ItemId, PeerId, PeerRowStage};
        let peer = PeerId::from_public_key(&[76; 32]).unwrap();
        let mut control = peer_control("principal-node", PeerRowStage::Ready);
        control.sender_label = "stored\nalias\u{1b}[31m".into();
        control.item_address = Some(ItemAddress::from_a2a_ingress(
            peer.clone(),
            ItemId::from_replay("alias-test"),
        ));
        let conversation = Conversation {
            messages: vec![peer_row("row", "draft")],
            peer_row_controls: [("row".to_owned(), control)].into_iter().collect(),
            ..Default::default()
        };
        let body = recipient_body(&conversation, &mut TuiState::new(130, 30), 130);
        let joined = body.join(" ");
        assert!(
            joined.contains("consultation")
                && joined.contains("stored")
                && joined.contains("alias"),
            "{body:?}"
        );
        assert!(joined.contains(peer.as_str()), "{body:?}");
        assert!(
            body.iter()
                .all(|line| !line.contains('\n') && !line.contains('\u{1b}')),
            "{body:?}"
        );
    }

    #[tokio::test]
    async fn story19_23_design_update_acknowledgement_changes_only_slot_one_while_received() {
        use crate::adapters::tui::state::ThreadAccess;
        use crate::domain::models::SemanticMessageType;
        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .unwrap();
        let address = append_received_test_item(
            &journal,
            "ri-design-slots",
            SemanticMessageType::DesignUpdate,
            "design body",
        )
        .await;
        let conversation = Conversation::default();
        let mut state = TuiState::new(80, 24);
        super::super::thread_open::open_received(
            workspace.path(),
            Some(&conversation),
            address.item().as_str(),
            &mut state,
            ThreadAccess::Writer,
            super::super::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        let actions = thread_inspector::actions(&state, Some(&conversation));
        assert_eq!(actions[1], Some(ThreadAction::Acknowledge));
        state.thread_inspector.as_mut().unwrap().message_type = SemanticMessageType::Unknown;
        let baseline = thread_inspector::actions(&state, Some(&conversation));
        for index in [0, 2, 3, 4, 5] {
            assert_eq!(actions[index], baseline[index]);
        }
        state.thread_inspector.as_mut().unwrap().message_type = SemanticMessageType::DesignUpdate;
        for item_state in ["acknowledged", "removed"] {
            state.thread_inspector.as_mut().unwrap().item_state = Some(item_state);
            assert_eq!(
                thread_inspector::actions(&state, Some(&conversation))[1],
                None
            );
        }
    }

    #[tokio::test]
    async fn story19_24_status_surface_slots_and_access_boundaries() {
        use crate::adapters::tui::state::ThreadAccess;
        use crate::domain::models::{
            PeerRowStage, SemanticMessageType, StatusDisclosure, StatusPreview,
        };
        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .unwrap();
        let address = append_received_test_item(
            &journal,
            "ri_status_slots",
            SemanticMessageType::StatusRequest,
            "status request",
        )
        .await;
        let mut control = peer_control("status-node", PeerRowStage::AwaitingOperator);
        control.item_address = Some(address.clone());
        control.message_type = SemanticMessageType::StatusRequest;
        let mut conversation = Conversation {
            messages: vec![peer_row("status-row", "waiting")],
            peer_row_controls: [("status-row".to_owned(), control)].into_iter().collect(),
            ..Default::default()
        };
        let mut state = TuiState::new(80, 24);
        super::super::thread_open::open_received(
            workspace.path(),
            Some(&conversation),
            address.item().as_str(),
            &mut state,
            ThreadAccess::Writer,
            super::super::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        // Compare against the unchanged no-preview surface at every other stage/type.
        for message_type in [
            SemanticMessageType::StatusRequest,
            SemanticMessageType::Consultation,
            SemanticMessageType::ArchitectureUpdate,
            SemanticMessageType::StoryAssignment,
            SemanticMessageType::BugReport,
            SemanticMessageType::Unknown,
        ] {
            for stage in [
                PeerRowStage::AwaitingOperator,
                PeerRowStage::Drafting,
                PeerRowStage::Ready,
                PeerRowStage::Settled,
            ] {
                for resolvable in [false, true] {
                    state.thread_inspector.as_mut().unwrap().message_type = message_type;
                    let control = conversation
                        .peer_row_controls
                        .get_mut("status-row")
                        .unwrap();
                    control.message_type = message_type;
                    control.stage = stage;
                    control.node_resolvable = resolvable;
                    control.status = None;
                    let baseline = thread_inspector::actions(&state, Some(&conversation));
                    conversation
                        .peer_row_controls
                        .get_mut("status-row")
                        .unwrap()
                        .status = Some(StatusPreview::new(StatusDisclosure::None, &[]));
                    let actions = thread_inspector::actions(&state, Some(&conversation));
                    // A restored (unresolvable) card keeps the status acts,
                    // as it keeps `[r] Respond`; the daemon's orphan arm answers.
                    if message_type == SemanticMessageType::StatusRequest
                        && stage == PeerRowStage::AwaitingOperator
                    {
                        assert!(
                            actions
                                == [
                                    Some(ThreadAction::Respond),
                                    Some(ThreadAction::Busy),
                                    Some(ThreadAction::ShareStatus),
                                    None,
                                    Some(ThreadAction::DeclineStatus),
                                    baseline[5],
                                ]
                        );
                    } else {
                        assert!(actions == baseline);
                    }
                }
            }
        }
        state.thread_inspector.as_mut().unwrap().message_type = SemanticMessageType::StatusRequest;
        let control = conversation
            .peer_row_controls
            .get_mut("status-row")
            .unwrap();
        control.stage = PeerRowStage::AwaitingOperator;
        control.node_resolvable = true;
        // Positive control: the writer slot of this exact state offers the status acts.
        let writer = thread_inspector::actions(&state, Some(&conversation));
        assert!(writer.contains(&Some(ThreadAction::ShareStatus)));
        assert!(writer.contains(&Some(ThreadAction::DeclineStatus)));
        for access in [ThreadAccess::ReadOnly, ThreadAccess::LocalOnly] {
            state.thread_inspector.as_mut().unwrap().access = access;
            assert!(
                thread_inspector::actions(&state, Some(&conversation))
                    .iter()
                    .all(Option::is_none)
            );
        }
        state.thread_inspector.as_mut().unwrap().access = ThreadAccess::Writer;
        state.thread_inspector.as_mut().unwrap().item_state = Some("removed");
        let actions = thread_inspector::actions(&state, Some(&conversation));
        assert!(!actions.contains(&Some(ThreadAction::ShareStatus)));
        assert!(!actions.contains(&Some(ThreadAction::DeclineStatus)));
    }

    #[tokio::test]
    async fn story19_24_status_keys_correlate_suppress_duplicates_and_release_failed_enqueue() {
        use crate::adapters::daemon::protocol::{RecipientAction, RecipientActionTarget};
        use crate::adapters::tui::state::{RecipientActionState, ThreadAccess};
        use crate::domain::models::{
            PeerRowStage, SemanticMessageType, StatusDisclosure, StatusPreview,
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for (key, decline) in [('s', false), ('n', true)] {
            // The keys go through the attached front door of a real receiver rig.
            let mut rig = attach_keys::tests::Rig::new(false);
            let workspace = rig.workspace().to_path_buf();
            let journal =
                crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
                    &workspace,
                )
                .await
                .unwrap();
            let address = append_received_test_item(
                &journal,
                "ri_status_keys",
                SemanticMessageType::StatusRequest,
                "status request",
            )
            .await;
            let mut control = peer_control("status-node", PeerRowStage::AwaitingOperator);
            control.item_address = Some(address.clone());
            control.message_type = SemanticMessageType::StatusRequest;
            control.status = Some(StatusPreview::new(StatusDisclosure::None, &[]));
            rig.domain_tab.conversation = Conversation {
                messages: vec![peer_row("status-row", "waiting")],
                peer_row_controls: [("status-row".to_owned(), control)].into_iter().collect(),
                ..Default::default()
            };
            super::super::thread_open::open_received(
                &workspace,
                Some(&rig.domain_tab.conversation),
                address.item().as_str(),
                &mut rig.state,
                ThreadAccess::Writer,
                super::super::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            rig.state.input_buffer = "unfinished operator text".to_owned();
            rig.state.cursor_position = rig.state.input_buffer.chars().count();
            rig.raw_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                .await;
            let frames = rig.frames();
            assert!(matches!(frames.as_slice(),
                [ClientFrame::ShareRecipientStatus { item_id, decline: actual }]
                    if item_id == "ri_status_keys" && *actual == decline));
            let action_key = (
                RecipientAction::ShareStatus,
                RecipientActionTarget::Item {
                    item_id: "ri_status_keys".to_owned(),
                },
            );
            assert!(matches!(
                rig.state.recipient_actions.get(&action_key),
                Some(RecipientActionState::Pending { .. })
            ));
            let inspector = rig.state.thread_inspector.as_ref().unwrap();
            let notices =
                crate::adapters::tui::handlers::recipient_actions::surface_action_notices(
                    &rig.state.recipient_actions,
                    &inspector.target,
                    inspector.node_id.as_deref(),
                )
                .map(|notice| notice.into_owned())
                .collect::<Vec<_>>();
            assert_eq!(notices, vec!["applying status reply for ri_status_keys…"]);
            let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
            assert!(actions[2].is_none() && actions[4].is_none());
            for duplicate in ['s', 'n'] {
                rig.raw_key(KeyEvent::new(KeyCode::Char(duplicate), KeyModifiers::NONE))
                    .await;
            }
            assert!(rig.frames().is_empty());
            assert_eq!(rig.state.input_buffer, "unfinished operator text");
            rig.state.recipient_actions.clear();
            rig.close_receiver();
            assert!(!queue_recipient_action(
                &mut rig.state,
                ClientFrame::ShareRecipientStatus {
                    item_id: "ri_status_keys".to_owned(),
                    decline
                },
                &rig.tx,
                false,
            ));
            assert!(!rig.state.recipient_actions.contains_key(&action_key));
            assert!(
                rig.state
                    .thread_outcomes
                    .lines("ri_status_keys")
                    .any(|notice| !notice.is_empty())
            );
            let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
            assert!(actions[2] == Some(ThreadAction::ShareStatus));
            assert!(actions[4] == Some(ThreadAction::DeclineStatus));
            assert_eq!(rig.state.input_buffer, "unfinished operator text");
        }
    }

    fn retro_test_preview() -> crate::domain::models::RetroPreview {
        crate::domain::models::RetroPreview {
            contribution: crate::domain::models::RetroContribution {
                received: [("consultation".to_owned(), 3)].into_iter().collect(),
                reply_minutes: vec![4, 25, 90],
                ack_minutes: vec![60],
            },
            nothing_in_window: false,
        }
    }

    /// Story 19.25 fixture: a journal-backed `retro_request` item in `workspace` and its
    /// pinned AwaitingOperator card with a stored preview.
    async fn retro_fixture(
        workspace: &Path,
        item_id: &str,
    ) -> (Conversation, crate::domain::models::ItemAddress) {
        use crate::domain::models::{PeerRowStage, SemanticMessageType};
        let journal =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(workspace)
                .await
                .unwrap();
        let address = append_received_test_item(
            &journal,
            item_id,
            SemanticMessageType::RetroRequest,
            "Sprint 7 retro",
        )
        .await;
        let mut control = peer_control("retro-node", PeerRowStage::AwaitingOperator);
        control.item_address = Some(address.clone());
        control.message_type = SemanticMessageType::RetroRequest;
        control.retro = Some(retro_test_preview());
        let conversation = Conversation {
            messages: vec![peer_row("retro-row", "waiting")],
            peer_row_controls: [("retro-row".to_owned(), control)].into_iter().collect(),
            ..Default::default()
        };
        (conversation, address)
    }

    /// Story 19.25 fixture: the retro item opened (by the production opener) as a Writer
    /// thread frame on a bare state.
    async fn open_retro_surface(item_id: &str) -> (tempfile::TempDir, Conversation, TuiState) {
        use crate::adapters::tui::state::ThreadAccess;
        let workspace = tempfile::tempdir().unwrap();
        let (conversation, address) = retro_fixture(workspace.path(), item_id).await;
        let mut state = TuiState::new(100, 40);
        super::super::thread_open::open_received(
            workspace.path(),
            Some(&conversation),
            address.item().as_str(),
            &mut state,
            ThreadAccess::Writer,
            super::super::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        (workspace, conversation, state)
    }

    /// The same fixture on the attached front-door rig (journal in the rig's workspace).
    async fn open_retro_rig(item_id: &str) -> attach_keys::tests::Rig {
        use crate::adapters::tui::state::ThreadAccess;
        let mut rig = attach_keys::tests::Rig::new(false);
        let workspace = rig.workspace().to_path_buf();
        let (conversation, address) = retro_fixture(&workspace, item_id).await;
        rig.domain_tab.conversation = conversation;
        rig.state.terminal_width = 100;
        rig.state.terminal_height = 40;
        super::super::thread_open::open_received(
            &workspace,
            Some(&rig.domain_tab.conversation),
            address.item().as_str(),
            &mut rig.state,
            ThreadAccess::Writer,
            super::super::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        rig
    }

    /// One printable key through the attached front door.
    async fn press_surface_key(rig: &mut attach_keys::tests::Rig, key: char) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        rig.raw_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
            .await;
    }

    #[tokio::test]
    async fn story19_25_retro_surface_slots_per_stage_type_and_restored_card() {
        use crate::adapters::tui::state::ThreadAccess;
        use crate::domain::models::{PeerRowStage, SemanticMessageType};
        let (_workspace, mut conversation, mut state) = open_retro_surface("ri_retro_slots").await;
        // Compare against the unchanged no-preview surface at every other stage/type.
        for message_type in [
            SemanticMessageType::RetroRequest,
            SemanticMessageType::StatusRequest,
            SemanticMessageType::Consultation,
            SemanticMessageType::ArchitectureUpdate,
            SemanticMessageType::StoryAssignment,
            SemanticMessageType::BugReport,
            SemanticMessageType::Unknown,
        ] {
            for stage in [
                PeerRowStage::AwaitingOperator,
                PeerRowStage::Drafting,
                PeerRowStage::Ready,
                PeerRowStage::Settled,
            ] {
                for resolvable in [false, true] {
                    state.thread_inspector.as_mut().unwrap().message_type = message_type;
                    let control = conversation.peer_row_controls.get_mut("retro-row").unwrap();
                    control.message_type = message_type;
                    control.stage = stage;
                    control.node_resolvable = resolvable;
                    control.retro = None;
                    let baseline = thread_inspector::actions(&state, Some(&conversation));
                    conversation
                        .peer_row_controls
                        .get_mut("retro-row")
                        .unwrap()
                        .retro = Some(retro_test_preview());
                    let actions = thread_inspector::actions(&state, Some(&conversation));
                    // A restored (unresolvable) card keeps the retro acts,
                    // as it keeps `[r] Respond` (19-24 BH-1).
                    if message_type == SemanticMessageType::RetroRequest
                        && stage == PeerRowStage::AwaitingOperator
                    {
                        assert_eq!(
                            actions,
                            [
                                Some(ThreadAction::Respond),
                                Some(ThreadAction::Busy),
                                Some(ThreadAction::RetroShare),
                                Some(ThreadAction::RetroLeaveOut),
                                Some(ThreadAction::RetroDecline),
                                baseline[5],
                            ],
                            "resolvable: {resolvable}"
                        );
                    } else {
                        assert_eq!(
                            actions, baseline,
                            "{message_type:?} {stage:?} resolvable: {resolvable}"
                        );
                    }
                }
            }
        }
        state.thread_inspector.as_mut().unwrap().message_type = SemanticMessageType::RetroRequest;
        let control = conversation.peer_row_controls.get_mut("retro-row").unwrap();
        control.message_type = SemanticMessageType::RetroRequest;
        control.stage = PeerRowStage::AwaitingOperator;
        control.node_resolvable = true;
        let no_overlay = std::collections::HashMap::new();
        assert!(
            thread_inspector::retro_overlay(
                &conversation,
                state.thread_inspector.as_ref().unwrap(),
                &no_overlay
            )
            .is_some_and(|overlay| overlay.left_out.is_none())
        );
        // Positive control: the writer slot of this exact state offers the retro acts.
        let retro_acts = [
            Some(ThreadAction::RetroShare),
            Some(ThreadAction::RetroLeaveOut),
            Some(ThreadAction::RetroDecline),
        ];
        let writer = thread_inspector::actions(&state, Some(&conversation));
        assert!(retro_acts.iter().all(|act| writer.contains(act)));
        for access in [ThreadAccess::ReadOnly, ThreadAccess::LocalOnly] {
            state.thread_inspector.as_mut().unwrap().access = access;
            assert!(
                thread_inspector::actions(&state, Some(&conversation))
                    .iter()
                    .all(Option::is_none)
            );
        }
        state.thread_inspector.as_mut().unwrap().access = ThreadAccess::Writer;
        state.thread_inspector.as_mut().unwrap().item_state = Some("removed");
        let actions = thread_inspector::actions(&state, Some(&conversation));
        assert!(retro_acts.iter().all(|act| !actions.contains(act)));
        assert!(
            thread_inspector::retro_overlay(
                &conversation,
                state.thread_inspector.as_ref().unwrap(),
                &no_overlay
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn story21_6_attached_retro_overlay_updates_frame_and_surface_and_sends() {
        use crate::adapters::daemon::protocol::{
            ProtocolError, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
        };
        use crate::adapters::tui::handlers::recipient_actions::{
            apply_action_result, prune_retro_left_out, surface_action_notices,
        };
        use crate::adapters::tui::state::RecipientActionState;
        use crate::domain::models::{PeerRowStage, RetroField, RetroFieldSet};
        let item = "ri_retro_keys";
        let mut rig = open_retro_rig(item).await;
        rig.state.input_buffer = "unfinished operator text".to_owned();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        let card = |rig: &mut attach_keys::tests::Rig| {
            recipient_body(&rig.domain_tab.conversation, &mut rig.state, 60).join("\n")
        };
        let paint = |rig: &mut attach_keys::tests::Rig| {
            paint_surface(&mut rig.state, &rig.domain_tab.conversation, 100, 40)
        };
        let overlay = |state: &TuiState| state.retro_left_out.get(item).cloned();
        let set = |fields: &[RetroField]| fields.iter().copied().collect::<RetroFieldSet>();

        assert!(card(&mut rig).contains("[2] reply times: 3 replies, median 25m"));
        let painted = paint(&mut rig);
        assert!(painted.contains("Shares (last 14 days):"), "{painted}");
        assert!(painted.contains("[s] Share") && !painted.contains("with redactions"));
        assert!(painted.contains("[1-3] Leave out") && painted.contains("[n] Decline"));

        // `[2]` leaves reply times out: the frame body and surface both reflect the overlay.
        press_surface_key(&mut rig, '2').await;
        assert!(rig.frames().is_empty());
        assert_eq!(overlay(&rig.state), Some(set(&[RetroField::ReplyTimes])));
        let text = card(&mut rig);
        assert!(text.contains("[2] reply times: left out"), "{text}");
        assert!(
            text.contains("Not shared: reply times · message content"),
            "{text}"
        );
        let painted = paint(&mut rig);
        assert!(painted.contains("[2] reply times: left out"), "{painted}");
        assert!(painted.contains("Not shared: reply times · message content"));
        assert!(painted.contains("[s] Share with redactions"), "{painted}");

        // Toggling back empties (and drops) the overlay.
        press_surface_key(&mut rig, '2').await;
        assert!(!rig.state.retro_left_out.contains_key(item));
        let painted = paint(&mut rig);
        assert!(!painted.contains("with redactions") && !painted.contains("left out"));

        // A field-level change under a non-empty overlay still re-renders.
        press_surface_key(&mut rig, '1').await;
        let painted = paint(&mut rig);
        assert!(painted.contains("[1] received items: left out"));
        press_surface_key(&mut rig, '3').await;
        let painted = paint(&mut rig);
        assert!(
            painted.contains("[3] acknowledgement times: left out"),
            "{painted}"
        );
        assert!(painted.contains("[2] reply times: 3 replies, median 25m"));
        for unowned in ['0', '4', '9'] {
            press_surface_key(&mut rig, unowned).await;
        }
        assert_eq!(
            overlay(&rig.state),
            Some(set(&[RetroField::Received, RetroField::AckTimes]))
        );
        assert!(rig.frames().is_empty());

        // `[s]` sends the overlay as the leave-out list, correlated.
        press_surface_key(&mut rig, 's').await;
        assert!(matches!(rig.frames().as_slice(),
            [ClientFrame::ShareRecipientRetro { item_id, leave_out, decline: false }]
                if item_id == item
                    && *leave_out == vec![RetroField::Received, RetroField::AckTimes]));
        let action_key = (
            RecipientAction::ShareRetro,
            RecipientActionTarget::Item {
                item_id: item.to_owned(),
            },
        );
        assert!(matches!(
            rig.state.recipient_actions.get(&action_key),
            Some(RecipientActionState::Pending { .. })
        ));
        let inspector = rig.state.thread_inspector.as_ref().unwrap();
        let notices = surface_action_notices(
            &rig.state.recipient_actions,
            &inspector.target,
            inspector.node_id.as_deref(),
        )
        .map(|notice| notice.into_owned())
        .collect::<Vec<_>>();
        assert_eq!(
            notices,
            vec!["applying retrospective reply for ri_retro_keys…"]
        );
        // Pending suppresses all three retro acts, toggles included.
        let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
        assert_eq!(actions[1], Some(ThreadAction::Busy));
        assert!(actions[2].is_none() && actions[3].is_none() && actions[4].is_none());
        for duplicate in ['2', 's', 'n'] {
            press_surface_key(&mut rig, duplicate).await;
        }
        assert!(rig.frames().is_empty());
        assert_eq!(
            overlay(&rig.state),
            Some(set(&[RetroField::Received, RetroField::AckTimes]))
        );
        assert_eq!(rig.state.input_buffer, "unfinished operator text");

        // Settling releases the overlay and the acts.
        apply_action_result(
            &mut rig.state,
            RecipientAction::ShareRetro,
            action_key.1.clone(),
            RecipientActionOutcome::Applied { reply: None },
        );
        assert!(rig.state.retro_left_out.is_empty());
        assert!(!card(&mut rig).contains("left out"));
        let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
        assert_eq!(actions[2], Some(ThreadAction::RetroShare));

        // `[n]` declines with no fields even over a non-empty overlay, which
        // a refusal keeps for the retry.
        press_surface_key(&mut rig, '2').await;
        press_surface_key(&mut rig, 'n').await;
        assert!(matches!(rig.frames().as_slice(),
            [ClientFrame::ShareRecipientRetro { item_id, leave_out, decline: true }]
                if item_id == item && leave_out.is_empty()));
        apply_action_result(
            &mut rig.state,
            RecipientAction::ShareRetro,
            action_key.1.clone(),
            RecipientActionOutcome::Refused {
                error: ProtocolError::ReadOnly,
            },
        );
        assert_eq!(overlay(&rig.state), Some(set(&[RetroField::ReplyTimes])));

        // A failed enqueue releases the pending key, not the overlay.
        rig.close_receiver();
        assert!(!queue_recipient_action(
            &mut rig.state,
            ClientFrame::ShareRecipientRetro {
                item_id: item.to_owned(),
                leave_out: vec![RetroField::ReplyTimes],
                decline: false,
            },
            &rig.tx,
            false,
        ));
        assert!(!rig.state.recipient_actions.contains_key(&action_key));
        assert!(
            rig.state
                .thread_outcomes
                .lines(item)
                .any(|notice| !notice.is_empty())
        );
        let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
        assert_eq!(actions[2], Some(ThreadAction::RetroShare));
        assert_eq!(overlay(&rig.state), Some(set(&[RetroField::ReplyTimes])));
        assert_eq!(rig.state.input_buffer, "unfinished operator text");

        // The card leaving takes the overlay and the surface preview with it.
        rig.domain_tab
            .conversation
            .peer_row_controls
            .get_mut("retro-row")
            .unwrap()
            .stage = PeerRowStage::Settled;
        prune_retro_left_out(&mut rig.state, &rig.domain_tab.conversation);
        assert!(rig.state.retro_left_out.is_empty());
        let painted = paint(&mut rig);
        assert!(!painted.contains("Shares (last 14 days):"), "{painted}");
        assert!(!painted.contains("[s] Share"));
    }

    /// Story 19.22 unit fixture: one received list item whose journal row
    /// carries the given story holders, so the page's `verdict_offer` is
    /// computed from real journal facts.
    async fn append_story_list_item(
        journal: &crate::infrastructure::subagent::node_journal::NodeJournal,
        item_id: &str,
        message_type: crate::domain::models::SemanticMessageType,
        holders: Vec<crate::domain::models::StoryHolder>,
    ) -> crate::domain::models::ItemAddress {
        use crate::domain::models::{
            ItemAddress, ItemId, PeerId, ReceivedStories, ReceivedStory, RoomEvent, StoryKey,
        };
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[74; 32]).unwrap(),
            ItemId::from_replay(item_id),
        );
        let stories = ReceivedStories::Listed {
            stories: holders
                .into_iter()
                .enumerate()
                .map(|(index, holder)| ReceivedStory {
                    key: StoryKey::parse(&format!("S7-{index:02}")).unwrap(),
                    holder,
                })
                .collect(),
        };
        journal
            .append_room(RoomEvent::RecipientItemReceived {
                stories: Some(stories),
                selected_for: None,
                address: address.clone(),
                task: format!("task-{item_id}"),
                alias: Some("sender".to_owned()),
                message_type,
                content: "scope body".to_owned(),
                in_reply_to: None,
                capability_hash: None,
                reply_capability_matched: false,
                authorship: crate::domain::models::Authorship::Unknown,
                send_authorization: crate::domain::models::SendAuthorization::Unknown,
            })
            .await
            .unwrap();
        address
    }

    #[tokio::test]
    async fn story19_22_verdict_surface_slots_per_stage_and_offer() {
        use crate::adapters::tui::state::ThreadAccess;
        use crate::domain::models::{PeerRowStage, SemanticMessageType, StoryHolder, VerdictOffer};
        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .unwrap();
        let you = append_story_list_item(
            &journal,
            "ri_verdict_you",
            SemanticMessageType::StoryAssignment,
            vec![StoryHolder::You],
        )
        .await;
        let mut state = TuiState::new(80, 24);
        crate::infrastructure::runtime::thread_open::open_received(
            workspace.path(),
            None,
            you.item().as_str(),
            &mut state,
            ThreadAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        let surface = state.thread_inspector.as_ref().unwrap();
        assert_eq!(surface.verdict_offer, VerdictOffer::AcceptOrDiscuss);
        // Writer, non-removed, no live draft: slots 0/1/2 per AC4(c).
        let actions = thread_inspector::actions(&state, Some(&Conversation::default()));
        assert_eq!(actions[0], Some(ThreadAction::DiscussConcerns));
        assert_eq!(actions[1], Some(ThreadAction::Acknowledge));
        assert_eq!(actions[2], Some(ThreadAction::Accept));

        state.thread_inspector.as_mut().unwrap().node_id = Some("verdict-node".to_owned());
        // Drafting: no verdict acts at all, the acknowledgement stays.
        for offer in [VerdictOffer::AcceptOrDiscuss, VerdictOffer::DiscussOnly] {
            state.thread_inspector.as_mut().unwrap().verdict_offer = offer;
            state.thread_inspector.as_mut().unwrap().item_state = Some("received");
            let mut conversation = Conversation {
                messages: vec![peer_row("verdict-row", "incoming")],
                peer_row_controls: [(
                    "verdict-row".to_owned(),
                    peer_control("verdict-node", PeerRowStage::Drafting),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            };
            conversation
                .peer_row_controls
                .get_mut("verdict-row")
                .unwrap()
                .item_address = Some(you.clone());
            let actions = thread_inspector::actions(&state, Some(&conversation));
            assert_eq!(actions[0], None, "Drafting offers no verdict");
            assert_eq!(actions[1], Some(ThreadAction::Acknowledge));
            assert_eq!(actions[2], None, "Drafting offers no accept");
        }

        // Ready never offers Accept (M11); its draft-review decision (Approve/Edit/Reject)
        // belongs to 21-6's RecipientDraft decision frame, not the thread window.
        state.thread_inspector.as_mut().unwrap().verdict_offer = VerdictOffer::AcceptOrDiscuss;
        let mut conversation = Conversation {
            messages: vec![peer_row("verdict-row", "incoming")],
            peer_row_controls: [(
                "verdict-row".to_owned(),
                peer_control("verdict-node", PeerRowStage::Ready),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        conversation
            .peer_row_controls
            .get_mut("verdict-row")
            .unwrap()
            .item_address = Some(you.clone());
        let actions = thread_inspector::actions(&state, Some(&conversation));
        assert_eq!(actions[1], Some(ThreadAction::Acknowledge));
        assert!(!actions.contains(&Some(ThreadAction::Accept)));
        conversation
            .peer_row_controls
            .get_mut("verdict-row")
            .unwrap()
            .node_resolvable = false;
        let actions = thread_inspector::actions(&state, Some(&conversation));
        assert!(
            !actions.contains(&Some(ThreadAction::Accept)),
            "Ready never offers Accept, including an orphan control"
        );

        // DiscussOnly offers concerns but no accept.
        state.thread_inspector.as_mut().unwrap().verdict_offer = VerdictOffer::DiscussOnly;
        let actions = thread_inspector::actions(&state, Some(&Conversation::default()));
        assert_eq!(actions[0], Some(ThreadAction::DiscussConcerns));
        assert_eq!(actions[2], None);

        // Offer None restores today's slot 0 by type.
        state.thread_inspector.as_mut().unwrap().verdict_offer = VerdictOffer::None;
        for (message_type, expected) in [
            (
                SemanticMessageType::ArchitectureUpdate,
                ThreadAction::Discuss,
            ),
            (SemanticMessageType::BugReport, ThreadAction::Discuss),
            (SemanticMessageType::Consultation, ThreadAction::Respond),
        ] {
            state.thread_inspector.as_mut().unwrap().message_type = message_type;
            let actions = thread_inspector::actions(&state, Some(&Conversation::default()));
            assert_eq!(actions[0], Some(expected), "{message_type:?}");
        }
        state.thread_inspector.as_mut().unwrap().message_type =
            SemanticMessageType::StoryAssignment;

        // Scope change received: slot 1 acknowledges (M12).
        let scope = append_story_list_item(
            &journal,
            "ri_verdict_scope",
            SemanticMessageType::ScopeChange,
            vec![StoryHolder::NoHolder],
        )
        .await;
        crate::infrastructure::runtime::thread_open::open_received(
            workspace.path(),
            None,
            scope.item().as_str(),
            &mut state,
            ThreadAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        assert_eq!(
            state.thread_inspector.as_ref().unwrap().verdict_offer,
            VerdictOffer::DiscussOnly,
            "a released list offers discuss only"
        );
        let actions = thread_inspector::actions(&state, Some(&Conversation::default()));
        assert_eq!(actions[1], Some(ThreadAction::Acknowledge));

        // Removed items and non-writer access offer nothing.
        state.thread_inspector.as_mut().unwrap().item_state = Some("removed");
        let actions = thread_inspector::actions(&state, Some(&Conversation::default()));
        assert!(actions[0].is_none() && actions[1].is_none() && actions[2].is_none());
        state.thread_inspector.as_mut().unwrap().item_state = Some("received");
        for access in [ThreadAccess::ReadOnly, ThreadAccess::LocalOnly] {
            state.thread_inspector.as_mut().unwrap().access = access;
            assert!(
                thread_inspector::actions(&state, Some(&Conversation::default()))
                    .iter()
                    .all(Option::is_none)
            );
        }
        // Positive control: the same state at Writer access offers the acts again.
        state.thread_inspector.as_mut().unwrap().access = ThreadAccess::Writer;
        assert!(
            thread_inspector::actions(&state, Some(&Conversation::default()))[1]
                == Some(ThreadAction::Acknowledge)
        );
    }

    #[tokio::test]
    async fn story19_22_verdict_keys_queue_frames_and_pending_suppresses_across_pages() {
        use crate::adapters::daemon::protocol::{RecipientAction, ReplyVerdict};
        use crate::adapters::tui::handlers::recipient_actions::action_is_pending;
        use crate::adapters::tui::state::{ThreadAccess, ThreadTarget};
        use crate::domain::models::{PeerRowStage, SemanticMessageType, StoryHolder};

        async fn reopen(
            rig: &mut attach_keys::tests::Rig,
            workspace: &Path,
            address: &crate::domain::models::ItemAddress,
        ) {
            crate::infrastructure::runtime::thread_open::open_received(
                workspace,
                Some(&rig.domain_tab.conversation),
                address.item().as_str(),
                &mut rig.state,
                ThreadAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
        }

        let mut rig = attach_keys::tests::Rig::new(false);
        let workspace = rig.workspace().to_path_buf();
        let journal =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(&workspace)
                .await
                .unwrap();
        let address = append_story_list_item(
            &journal,
            "ri_verdict_keys",
            SemanticMessageType::StoryAssignment,
            vec![StoryHolder::You],
        )
        .await;
        let mut control = peer_control("verdict-node", PeerRowStage::Settled);
        control.item_address = Some(address.clone());
        rig.domain_tab.conversation = Conversation {
            messages: vec![peer_row("verdict-row", "incoming")],
            peer_row_controls: [("verdict-row".to_owned(), control)].into_iter().collect(),
            ..Default::default()
        };
        rig.state.input_buffer = String::new();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        reopen(&mut rig, &workspace, &address).await;
        let opened_id = address.item().as_str().to_owned();

        // `[y] Accept` queues the verdict frame with empty text.
        press_surface_key(&mut rig, 'y').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::ReplyToRecipientItem { item_id, text, verdict: Some(ReplyVerdict::Accept) }]
                if *item_id == opened_id && text.is_empty()
        ));
        assert!(action_is_pending(
            &rig.state,
            RecipientAction::Reply,
            &ThreadTarget::Item(address.clone()),
            None,
        ));

        // A pending Reply suppresses both verdict acts; the suppression
        // survives context selection and a newer page (cache re-render).
        let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
        assert_eq!(actions[0], None, "pending Reply suppresses DiscussConcerns");
        assert_eq!(actions[2], None, "pending Reply suppresses Accept");
        rig.state
            .thread_inspector
            .as_mut()
            .unwrap()
            .context_selected = true;
        rig.state.recipient_context_ids.insert(opened_id.clone());
        let suppressed = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
        assert_eq!(suppressed[0], None);
        assert_eq!(suppressed[2], None);
        let painted = paint_received_surface(&mut rig.state);
        assert!(painted.contains("[u] Stop using as context"), "{painted}");
        assert!(!painted.contains("[y] Accept"));
        assert!(!painted.contains("[d] Discuss concerns"));

        // Refresh from a genuinely newer journal page while Reply is still
        // pending; neither verdict hint may be restored by the new content.
        journal
            .append_room(crate::domain::models::RoomEvent::RecipientItemReplied {
                retro: None,
                address: crate::domain::models::TurnAddress::answering(
                    &address,
                    crate::domain::models::ReplyTurnId::from_replay("rr-newer"),
                ),
                answers: address.item().clone(),
                content: "newer turn arrived".into(),
                authorship: crate::domain::models::Authorship::HumanWritten,
                send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
                kind: crate::domain::models::ReplyKind::Answer,
                via_node: None,
                status: None,
            })
            .await
            .unwrap();
        reopen(&mut rig, &workspace, &address).await;
        assert!(
            rig.state
                .thread_inspector
                .as_ref()
                .unwrap()
                .content
                .contains("newer turn arrived")
        );
        let painted = paint_received_surface(&mut rig.state);
        assert!(!painted.contains("[y] Accept"));
        assert!(!painted.contains("[d] Discuss concerns"));

        // A newer page repaints with the verdict acts restored once the
        // pending action clears — the widened u32 mask tracks both.
        rig.state.recipient_actions.clear();
        reopen(&mut rig, &workspace, &address).await;
        let painted = paint_received_surface(&mut rig.state);
        assert!(painted.contains("[y] Accept"), "{painted}");
        assert!(painted.contains("[d] Discuss concerns"));
        assert!(painted.contains("[a] Acknowledge"));

        // `[d] Discuss concerns` opens the typed verdict in the thread's own field.
        let composer = rig.state.input_buffer.clone();
        press_surface_key(&mut rig, 'd').await;
        let inspector = rig.state.thread_inspector.as_ref().unwrap();
        let reply = inspector.reply.as_ref().expect("reply field");
        assert_eq!(
            reply.prompt(&opened_id),
            format!("/team reply --discuss {opened_id} ")
        );
        assert_eq!(reply.field.as_str(), "");
        assert_eq!(rig.state.input_buffer, composer);
    }

    #[test]
    fn story21_6_attached_status_frame_preview_wraps_and_caps_stories() {
        use crate::domain::models::{
            HeldStory, PeerRowStage, ReceiptState, StatusDisclosure, StatusPreview, StoryKey,
        };
        let held = (0..64)
            .map(|index| HeldStory {
                key: StoryKey::parse(&format!("S7-{index:02}")).unwrap(),
                state: ReceiptState::Acknowledged,
            })
            .collect::<Vec<_>>();
        for (level, stories, expected) in [
            (
                StatusDisclosure::None,
                held.as_slice(),
                "Shares nothing: your sharing level is none",
            ),
            (
                StatusDisclosure::Minimal,
                &[][..],
                "Shares nothing: no stories are assigned to you on this host",
            ),
            (
                StatusDisclosure::Minimal,
                held.as_slice(),
                "Shares (minimal): S7-00",
            ),
            (
                StatusDisclosure::Status,
                held.as_slice(),
                "Shares (status): S7-00 acknowledged",
            ),
        ] {
            let mut control = peer_control("status", PeerRowStage::AwaitingOperator);
            control.status = Some(StatusPreview::new(level, stories));
            let conversation = Conversation {
                messages: vec![peer_row("status", "private request body")],
                peer_row_controls: [("status".to_owned(), control)].into_iter().collect(),
                ..Default::default()
            };
            let text = recipient_body(&conversation, &mut TuiState::new(80, 24), 60);
            assert!(
                text.iter()
                    .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 60),
                "{text:?}"
            );
            let joined = text
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(joined.contains(expected), "{joined}");
            assert!(
                joined.contains("Not shared: conversations, plans, files, activity"),
                "{joined}"
            );
            if level != StatusDisclosure::None && !stories.is_empty() {
                for story in stories.iter().take(40) {
                    assert!(joined.contains(story.key.as_str()), "{joined}");
                }
                assert!(joined.contains("… and 24 more"), "{joined}");
                assert!(!joined.contains(stories[40].key.as_str()), "{joined}");
            }
            assert!(!joined.contains("private request body"));
        }
    }

    #[test]
    fn story21_6_attached_retro_frame_preview_wraps_and_reflects_redactions() {
        use crate::domain::models::{
            PeerRowStage, RetroContribution, RetroField, RetroFieldSet, RetroPreview,
        };
        let contribution = RetroContribution {
            received: [
                ("consultation".to_owned(), 3),
                ("story_assignment".to_owned(), 2),
            ]
            .into_iter()
            .collect(),
            reply_minutes: vec![10, 20, 25, 30, 40],
            ack_minutes: Vec::new(),
        };
        let full = contribution.text_lines();
        let not_shared_tail =
            "message content, senders, your sends, conversations, plans, files, activity";
        let render = |preview: RetroPreview, left_out: Option<&RetroFieldSet>| {
            let mut control = peer_control("retro", PeerRowStage::AwaitingOperator);
            control.retro = Some(preview);
            let address = crate::domain::models::ItemAddress::from_a2a_ingress(
                crate::domain::models::PeerId::from_public_key(&[75; 32]).unwrap(),
                crate::domain::models::ItemId::from_replay("retro-preview"),
            );
            control.item_address = Some(address.clone());
            let conversation = Conversation {
                messages: vec![peer_row("retro", "private request body")],
                peer_row_controls: [("retro".to_owned(), control)].into_iter().collect(),
                ..Default::default()
            };
            let mut state = TuiState::new(80, 24);
            if let Some(fields) = left_out {
                state
                    .retro_left_out
                    .insert(address.item().as_str().to_owned(), fields.clone());
            }
            let body = recipient_body(&conversation, &mut state, 60);
            let first_preview = body
                .iter()
                .position(|line| line.starts_with("Shares"))
                .unwrap();
            let mut text = vec![body[0].clone()];
            text.extend_from_slice(&body[first_preview..]);
            assert!(
                text.iter()
                    .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 60),
                "{text:?}"
            );
            assert!(!text.join(" ").contains("private request body"));
            text
        };
        let joined = |lines: &[String]| {
            lines
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };

        let shared = render(
            RetroPreview {
                contribution: contribution.clone(),
                nothing_in_window: false,
            },
            None,
        );
        assert_eq!(shared[1], "Shares (last 14 days):");
        assert_eq!(
            shared[2],
            "[1] received items: consultation 3 · story_assignment 2"
        );
        assert_eq!(shared[3], "[2] reply times: 5 replies, median 25m");
        assert_eq!(shared[4], "[3] acknowledgement times: none");
        assert_eq!(full[1], "[2] reply times: 5 replies, median 25m");
        let tail = joined(&shared[5..]);
        assert!(
            tail.starts_with(&format!("Not shared: {not_shared_tail}")),
            "{tail}"
        );
        assert!(shared[5].starts_with("Not shared: message content,"));

        let left_out: RetroFieldSet = [RetroField::ReplyTimes].into_iter().collect();
        let redacted = render(
            RetroPreview {
                contribution: contribution.clone(),
                nothing_in_window: false,
            },
            Some(&left_out),
        );
        assert_eq!(redacted[2], shared[2]);
        assert_eq!(redacted[3], "[2] reply times: left out");
        assert_eq!(redacted[4], shared[4]);
        assert!(redacted[5].starts_with("Not shared: reply times · message content,"));
        assert!(
            joined(&redacted[5..])
                .starts_with(&format!("Not shared: reply times · {not_shared_tail}")),
            "{redacted:?}"
        );

        let all: RetroFieldSet = RetroField::ALL.into_iter().collect();
        let everything = render(
            RetroPreview {
                contribution: contribution.clone(),
                nothing_in_window: false,
            },
            Some(&all),
        );
        assert_eq!(everything[2], "[1] received items: left out");
        assert_eq!(everything[3], "[2] reply times: left out");
        assert_eq!(everything[4], "[3] acknowledgement times: left out");
        assert!(
            joined(&everything[5..]).starts_with(&format!(
                "Not shared: received items · reply times · acknowledgement times · {not_shared_tail}"
            )),
            "{everything:?}"
        );

        let nothing = render(
            RetroPreview {
                contribution: RetroContribution::nothing(),
                nothing_in_window: true,
            },
            None,
        );
        let nothing_text = joined(&nothing);
        assert!(
            nothing_text.contains(&format!(
                "Shares nothing: no team messages reached this host in the last 14 days Not shared: {not_shared_tail}"
            )),
            "{nothing_text}"
        );
        assert!(!nothing_text.contains("Shares (last 14 days):"));
        assert!(nothing.iter().all(|line| !line.contains("[1]")));

        // Narrower than 60 columns the preview still wraps inside the frame.
        let mut narrow = peer_control("retro", PeerRowStage::AwaitingOperator);
        narrow.retro = Some(RetroPreview {
            contribution: contribution.clone(),
            nothing_in_window: false,
        });
        let conversation = Conversation {
            messages: vec![peer_row("retro", "private request body")],
            peer_row_controls: [("retro".to_owned(), narrow)].into_iter().collect(),
            ..Default::default()
        };
        let narrow = recipient_body(&conversation, &mut TuiState::new(80, 24), 40);
        let preview = narrow
            .iter()
            .skip(1)
            .take(narrow.len() - 2)
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            preview
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 40),
            "{narrow:?}"
        );
        assert!(
            joined(&preview[..]).ends_with(&format!("Not shared: {not_shared_tail}")),
            "{narrow:?}"
        );

        let mut plain = peer_control("plain", PeerRowStage::AwaitingOperator);
        plain.retro = None;
        let conversation = Conversation {
            messages: vec![peer_row("plain", "body")],
            peer_row_controls: [("plain".to_owned(), plain)].into_iter().collect(),
            ..Default::default()
        };
        let plain_text = joined(&recipient_body(
            &conversation,
            &mut TuiState::new(80, 24),
            60,
        ));
        assert!(!plain_text.contains("Shares"), "{plain_text}");
        assert!(!plain_text.contains("Not shared"), "{plain_text}");
    }

    #[test]
    fn story21_6_attached_story_summary_uses_control_at_floor_width() {
        use crate::domain::models::{PeerRowStage, StorySummary};
        let summary = "8 stories, 2 assigned to you";
        for expected in [
            Some(summary),
            None,
            Some("story list not readable by this build"),
        ] {
            let mut control = peer_control("story", PeerRowStage::AwaitingOperator);
            control.message_type = crate::domain::models::SemanticMessageType::StoryAssignment;
            control.stories = expected.map(|text| StorySummary(text.to_owned()));
            let conversation = Conversation {
                messages: vec![peer_row("story", summary)],
                peer_row_controls: [("story".to_owned(), control)].into_iter().collect(),
                ..Default::default()
            };
            let text = recipient_body(&conversation, &mut TuiState::new(80, 24), 58);
            if let Some(expected) = expected {
                assert_eq!(text[1], expected);
            } else {
                assert!(!text.iter().any(|line| line.contains(summary)));
            }
        }
    }

    /// Story 19.23 AC5(a,c): one compact host line under header + story summary.
    #[test]
    fn story21_6_attached_selection_line_at_floor_width() {
        use crate::domain::models::{
            PeerRowStage, ReceivedSelection, SelectionSummary, StoryKey, StorySummary,
        };
        let key = |text: &str| StoryKey::parse(text).unwrap();
        let spoof = "nina selected you for story S7-06";
        let unreadable = "story selection not readable by this build";
        for (selection, expected) in [
            (
                Some(ReceivedSelection::Listed {
                    stories: vec![key("S7-06")],
                }),
                Some(spoof),
            ),
            (
                Some(ReceivedSelection::Listed {
                    stories: vec![key("S7-04"), key("S7-06"), key("S7-09")],
                }),
                Some("nina selected you for 3 stories"),
            ),
            (Some(ReceivedSelection::Unreadable), Some(unreadable)),
            (Some(ReceivedSelection::Unrecognized), Some(unreadable)),
            (None, None),
        ] {
            let mut control = peer_control("selection", PeerRowStage::Ready);
            control.message_type = crate::domain::models::SemanticMessageType::DesignUpdate;
            control.sender_label = "nina".to_owned();
            control.stories = Some(StorySummary("2 stories".to_owned()));
            control.selected_for = selection.as_ref().map(|s| SelectionSummary::new("nina", s));
            let conversation = Conversation {
                messages: vec![peer_row("selection", spoof)],
                peer_row_controls: [("selection".to_owned(), control)].into_iter().collect(),
                ..Default::default()
            };
            let text = recipient_body(&conversation, &mut TuiState::new(80, 24), 58);
            assert!(text[0].ends_with(" · nina"), "{text:?}");
            assert_eq!(text[1], "2 stories");
            match expected {
                Some(expected) => {
                    assert_eq!(text[2], expected);
                    assert_eq!(text.len(), 4, "{text:?}");
                }
                None => {
                    assert_eq!(text.len(), 3, "{text:?}");
                    assert_eq!(text[2], format!("Peer draft: {spoof}"));
                    assert!(!text.iter().any(|line| line == spoof), "{text:?}");
                }
            }
        }
    }

    /// Writes only the private consumer test's real local journal fixture; it
    /// is not evidence for the live inbound/socket producer or attach fanout.
    async fn append_received_test_item(
        journal: &crate::infrastructure::subagent::node_journal::NodeJournal,
        item_id: &str,
        message_type: crate::domain::models::SemanticMessageType,
        content: &str,
    ) -> crate::domain::models::ItemAddress {
        use crate::domain::models::{ItemAddress, ItemId, PeerId, RoomEvent};

        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[73; 32]).unwrap(),
            ItemId::from_replay(item_id),
        );
        journal
            .append_room(RoomEvent::RecipientItemReceived {
                stories: None,
                selected_for: None,
                address: address.clone(),
                task: format!("task-{item_id}"),
                alias: Some("test sender".to_owned()),
                message_type,
                content: content.to_owned(),
                in_reply_to: None,
                capability_hash: None,
                reply_capability_matched: false,
                authorship: crate::domain::models::Authorship::Unknown,
                send_authorization: crate::domain::models::SendAuthorization::Unknown,
            })
            .await
            .unwrap();
        address
    }

    /// Replaces `explicit_response_focus_freezes_input_and_filters_action_events` (node
    /// surface: Reject-by-`n` on an explicit node target and the FIFO-head retarget check
    /// are DELETED, PC-13 — 21-6's decision frame owns draft decisions). What stays is the
    /// raw-key filter and the frozen composer, now on the thread frame.
    #[tokio::test]
    async fn thread_frame_freezes_input_and_filters_raw_action_events() {
        use crate::adapters::tui::presentation::{self, Surface};
        use crate::adapters::tui::state::ThreadAccess;
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let mut rig = attach_keys::tests::Rig::new(false);
        let workspace = rig.workspace().to_path_buf();
        let journal =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(&workspace)
                .await
                .unwrap();
        let address = append_received_test_item(
            &journal,
            "ri_raw_filter",
            crate::domain::models::SemanticMessageType::Consultation,
            "A locally journaled consultation.",
        )
        .await;
        let item_id = address.item().as_str().to_owned();
        crate::infrastructure::runtime::thread_open::open_received(
            &workspace,
            Some(&rig.domain_tab.conversation),
            &item_id,
            &mut rig.state,
            ThreadAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        assert!(presentation::is_open(&rig.state, Surface::ThreadInspector));
        presentation::reducer::set_base(&mut rig.state, FocusState::Input);
        // Positive control for the filter below: the offered writer action is `[r] Respond`.
        assert_eq!(
            thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation))[0],
            Some(ThreadAction::Respond)
        );
        for key in [
            KeyEvent::new_with_kind(KeyCode::Char('r'), KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new_with_kind(
                KeyCode::Char('r'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            rig.raw_key(key).await;
        }
        assert!(
            rig.frames().is_empty(),
            "non-Press and ineligible keys cannot dispatch"
        );
        assert_eq!(
            rig.state.input_buffer, "",
            "no filtered key reaches the composer or prefills a reply"
        );
        assert!(presentation::is_open(&rig.state, Surface::ThreadInspector));

        // A plain Press opens the frame-owned field despite the nonempty composer.
        rig.state.input_buffer = "unfinished operator text".to_owned();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        rig.raw_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .await;
        assert!(rig.frames().is_empty());
        assert_eq!(rig.state.input_buffer, "unfinished operator text");
        assert_eq!(
            rig.state
                .thread_inspector
                .as_ref()
                .unwrap()
                .reply
                .as_ref()
                .unwrap()
                .field
                .as_str(),
            ""
        );
        rig.raw_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(rig.state.thread_inspector.as_ref().unwrap().reply.is_none());
        rig.raw_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(rig.state.thread_inspector.is_none());
        assert!(!presentation::is_open(&rig.state, Surface::ThreadInspector));
        assert_eq!(rig.state.focus, FocusState::Input);
        assert_eq!(rig.state.input_buffer, "unfinished operator text");
    }

    #[tokio::test]
    async fn answerable_other_and_unknown_items_respond_without_discuss() {
        use crate::adapters::tui::presentation::{self, Surface};
        use crate::adapters::tui::state::{ThreadAccess, ThreadTarget};
        use crate::domain::models::{PeerRowStage, SemanticMessageType};

        let mut rig = attach_keys::tests::Rig::new(false);
        let workspace = rig.workspace().to_path_buf();
        let journal =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(&workspace)
                .await
                .unwrap();
        let mut conversation = Conversation::default();
        let items = [
            ("ri_story_assignment", SemanticMessageType::StoryAssignment),
            ("ri_future_kind", SemanticMessageType::Unknown),
        ];
        let mut addresses = Vec::new();
        for (index, (item_id, message_type)) in items.into_iter().enumerate() {
            let address = append_received_test_item(
                &journal,
                item_id,
                message_type,
                "A locally journaled answerable item.",
            )
            .await;
            let row_id = format!("peer-row-{index}");
            conversation
                .messages
                .push(peer_row(&row_id, "pending response"));
            conversation.peer_row_controls.insert(
                row_id,
                crate::domain::models::PeerRowControl {
                    node_id: format!("node-{index}"),
                    item_address: Some(address.clone()),
                    stories: None,
                    status: None,
                    selected_for: None,
                    retro: None,
                    message_type,
                    sender_label: "test sender".to_owned(),
                    stage: PeerRowStage::AwaitingOperator,
                    node_resolvable: true,
                },
            );
            addresses.push(address);
        }

        rig.domain_tab.conversation = conversation;
        rig.state.input_buffer = String::new();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        presentation::reducer::set_base(&mut rig.state, FocusState::Input);
        for address in addresses {
            rig.state.input_buffer.push_str("unfinished operator text");
            crate::infrastructure::runtime::thread_open::open_received(
                &workspace,
                Some(&rig.domain_tab.conversation),
                address.item().as_str(),
                &mut rig.state,
                ThreadAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            let surface = rig.state.thread_inspector.as_ref().unwrap();
            assert!(matches!(
                &surface.target,
                ThreadTarget::Item(opened) if opened == &address
            ));
            assert!(presentation::is_open(&rig.state, Surface::ThreadInspector));
            assert_eq!(
                rig.state.focus,
                FocusState::Input,
                "opening the thread frame never writes base focus"
            );
            assert_eq!(rig.state.input_buffer, "unfinished operator text");
            let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
            assert!(matches!(actions[0], Some(ThreadAction::Respond)));
            assert!(
                !actions.contains(&Some(ThreadAction::Discuss)),
                "other and unknown semantic types have no Discuss action"
            );
            press_surface_key(&mut rig, 'r').await;
            assert!(
                rig.frames().is_empty(),
                "nonempty input must be preserved rather than overwritten"
            );
            assert_eq!(rig.state.input_buffer, "unfinished operator text");
            assert!(rig.state.thread_inspector.is_some());
            assert_eq!(
                rig.state
                    .thread_inspector
                    .as_ref()
                    .unwrap()
                    .reply
                    .as_ref()
                    .unwrap()
                    .prompt(address.item().as_str()),
                format!("/team reply {} ", address.item().as_str())
            );
            rig.raw_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ))
            .await;
            assert!(rig.state.thread_inspector.as_ref().unwrap().reply.is_none());
            rig.raw_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ))
            .await;
            assert!(rig.state.thread_inspector.is_none());
            assert_eq!(rig.state.focus, FocusState::Input);
            assert_eq!(rig.state.input_buffer, "unfinished operator text");

            rig.state.input_buffer.clear();
            crate::infrastructure::runtime::thread_open::open_received(
                &workspace,
                Some(&rig.domain_tab.conversation),
                address.item().as_str(),
                &mut rig.state,
                ThreadAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
            assert!(matches!(actions[0], Some(ThreadAction::Respond)));
            press_surface_key(&mut rig, 'r').await;
            assert!(rig.frames().is_empty());
            let reply = rig
                .state
                .thread_inspector
                .as_ref()
                .unwrap()
                .reply
                .as_ref()
                .unwrap();
            assert_eq!(
                reply.prompt(address.item().as_str()),
                format!("/team reply {} ", address.item().as_str())
            );
            assert_eq!(reply.field.as_str(), "");
            assert!(rig.state.input_buffer.is_empty());
            assert_eq!(rig.state.focus, FocusState::Input);
            rig.raw_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ))
            .await;
            rig.raw_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ))
            .await;
        }
    }

    #[tokio::test]
    async fn story_assignment_surface_a_key_dispatches_ack_only_for_received_writer() {
        use crate::adapters::daemon::protocol::ClientFrame;
        use crate::adapters::tui::state::ThreadAccess;
        use crate::domain::models::SemanticMessageType;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for message_type in [
            SemanticMessageType::StoryAssignment,
            SemanticMessageType::BugReport,
        ] {
            let mut rig = attach_keys::tests::Rig::new(false);
            let workspace = rig.workspace().to_path_buf();
            let journal =
                crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
                    &workspace,
                )
                .await
                .unwrap();
            let address = append_received_test_item(
                &journal,
                "ri_story_assignment",
                message_type,
                "A locally journaled story assignment.",
            )
            .await;
            let row_id = "story-assignment-row".to_owned();
            rig.domain_tab.conversation = Conversation {
                messages: vec![peer_row(&row_id, "pending story assignment")],
                peer_row_controls: [(
                    row_id,
                    crate::domain::models::PeerRowControl {
                        node_id: "story-assignment-node".to_owned(),
                        item_address: Some(address.clone()),
                        stories: None,
                        status: None,
                        selected_for: None,
                        retro: None,
                        message_type,
                        sender_label: "test sender".to_owned(),
                        stage: crate::domain::models::PeerRowStage::AwaitingOperator,
                        node_resolvable: true,
                    },
                )]
                .into_iter()
                .collect(),
                ..Conversation::default()
            };
            rig.state.input_buffer = String::new();
            rig.state.cursor_position = rig.state.input_buffer.chars().count();

            crate::infrastructure::runtime::thread_open::open_received(
                &workspace,
                Some(&rig.domain_tab.conversation),
                address.item().as_str(),
                &mut rig.state,
                ThreadAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            let actions = thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
            assert!(actions.contains(&Some(ThreadAction::Acknowledge)));
            rig.raw_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await;
            assert!(
                rig.frames().is_empty(),
                "opening/viewing a received story assignment does not acknowledge it"
            );

            press_surface_key(&mut rig, 'a').await;
            assert!(matches!(
                rig.frames().as_slice(),
                [ClientFrame::AcknowledgeRecipientItem { item_id }]
                    if item_id == address.item().as_str()
            ));
            assert!(
                thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation))
                    .contains(&Some(ThreadAction::Context)),
                "after acknowledgement, the open surface still offers [u] Use as context"
            );
            press_surface_key(&mut rig, 'u').await;
            assert!(matches!(
                rig.frames().as_slice(),
                [ClientFrame::SetRecipientContext {
                    item_id,
                    selected: true
                }] if item_id == address.item().as_str()
            ));
            crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                &mut rig.state,
                crate::adapters::daemon::protocol::RecipientAction::ContextSelect,
                crate::adapters::daemon::protocol::RecipientActionTarget::Item {
                    item_id: address.item().as_str().to_owned(),
                },
                crate::adapters::daemon::protocol::RecipientActionOutcome::ContextSelection {
                    selected: true,
                    changed: true,
                },
            );
            press_surface_key(&mut rig, 'u').await;
            assert!(matches!(
                rig.frames().as_slice(),
                [ClientFrame::SetRecipientContext {
                    item_id,
                    selected: false
                }] if item_id == address.item().as_str()
            ));
            crate::infrastructure::runtime::thread_open::open_received(
                &workspace,
                Some(&rig.domain_tab.conversation),
                address.item().as_str(),
                &mut rig.state,
                ThreadAccess::ReadOnly,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            assert!(
                !thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation))
                    .contains(&Some(ThreadAction::Acknowledge)),
                "read-only item surfaces do not offer acknowledgement"
            );
            press_surface_key(&mut rig, 'a').await;
            assert!(
                rig.frames().is_empty(),
                "the acknowledgement key is a consumed no-op in a read-only thread"
            );

            crate::infrastructure::runtime::thread_open::open_received(
                &workspace,
                Some(&rig.domain_tab.conversation),
                address.item().as_str(),
                &mut rig.state,
                ThreadAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            if let Some(surface) = rig.state.thread_inspector.as_mut() {
                surface.item_state = Some("removed");
            }
            assert!(
                !thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation))
                    .contains(&Some(ThreadAction::Acknowledge)),
                "removed items do not offer acknowledgement"
            );
        }
    }

    #[tokio::test]
    async fn story19_20_bug_report_discuss_survives_settled_or_absent_control() {
        use crate::adapters::tui::state::ThreadAccess;
        use crate::domain::models::{PeerRowControl, PeerRowStage, SemanticMessageType};
        let mut rig = attach_keys::tests::Rig::new(false);
        let workspace = rig.workspace().to_path_buf();
        let journal =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(&workspace)
                .await
                .unwrap();
        let address = append_received_test_item(
            &journal,
            "ri_bug_discuss",
            SemanticMessageType::BugReport,
            "bug report",
        )
        .await;
        for stage in [
            None,
            Some(PeerRowStage::Settled),
            Some(PeerRowStage::Drafting),
        ] {
            let mut conversation = Conversation::default();
            if let Some(stage) = stage {
                let row = "peer-response-bug";
                conversation.messages.push(peer_row(row, "response"));
                conversation.peer_row_controls.insert(
                    row.into(),
                    PeerRowControl {
                        node_id: "bug".into(),
                        item_address: Some(address.clone()),
                        stories: None,
                        status: None,
                        selected_for: None,
                        retro: None,
                        message_type: SemanticMessageType::BugReport,
                        sender_label: "sender".into(),
                        stage,
                        node_resolvable: stage == PeerRowStage::Drafting,
                    },
                );
            }
            rig.domain_tab.conversation = conversation;
            for access in [
                ThreadAccess::Writer,
                ThreadAccess::ReadOnly,
                ThreadAccess::LocalOnly,
            ] {
                crate::infrastructure::runtime::thread_open::open_received(
                    &workspace,
                    Some(&rig.domain_tab.conversation),
                    address.item().as_str(),
                    &mut rig.state,
                    access,
                    crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
                )
                .await
                .unwrap();
                let actions =
                    thread_inspector::actions(&rig.state, Some(&rig.domain_tab.conversation));
                if access != ThreadAccess::Writer {
                    assert!(actions.iter().all(Option::is_none));
                    continue;
                }
                assert!(matches!(actions[1], Some(ThreadAction::Acknowledge)));
                if stage == Some(PeerRowStage::Drafting) {
                    assert!(actions[0].is_none());
                    continue;
                }
                assert!(matches!(actions[0], Some(ThreadAction::Discuss)));
                rig.state.input_buffer = String::new();
                rig.state.cursor_position = rig.state.input_buffer.chars().count();
                press_surface_key(&mut rig, 'd').await;
                assert_eq!(rig.state.input_buffer, "");
                let reply = rig
                    .state
                    .thread_inspector
                    .as_ref()
                    .unwrap()
                    .reply
                    .as_ref()
                    .unwrap();
                assert_eq!(
                    reply.prompt("ri_bug_discuss"),
                    "/team reply ri_bug_discuss "
                );
                assert!(
                    rig.frames().is_empty(),
                    "Discuss composes, it does not send"
                );
                assert!(rig.state.thread_inspector.is_some());
            }
        }
    }

    #[test]
    fn story21_6_attached_ordinary_enter_remains_a_model_turn_with_queued_recipient() {
        use crate::domain::models::PeerRowStage;
        use crossterm::event::KeyCode;

        let conversation = Conversation {
            messages: vec![peer_row("pending-card", "awaiting response")],
            peer_row_controls: [(
                "pending-card".to_owned(),
                peer_control("node-pending", PeerRowStage::AwaitingOperator),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let mut state = TuiState::new(80, 24);
        crate::adapters::tui::decision_queue::sync_recipient_drafts(
            &mut state,
            &conversation,
            false,
        );
        assert_eq!(state.decisions.len(), 1);
        state.input_buffer = "ordinary model question".to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, false, &mut state, &tx),
            ComposerOutcome::Submit(text) if text == "ordinary model question"
        ));
        assert!(rx.try_recv().is_err());
        assert_eq!(state.decisions.len(), 1);
    }

    fn paint_received_surface(state: &mut TuiState) -> String {
        paint_surface(state, &Conversation::default(), 80, 24)
    }

    /// Paints the thread frame through the production attached compositor (`paint_attached`
    /// → `presentation::compose`); the frame's layout cache persists across calls, as across
    /// frames.
    fn paint_surface(
        state: &mut TuiState,
        conversation: &Conversation,
        width: u16,
        height: u16,
    ) -> String {
        state.terminal_width = width;
        state.terminal_height = height;
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let tab =
            crate::domain::models::tab::TabState::from_conversation(0, Conversation::default());
        let mut render_state = TabRenderState::default();
        terminal
            .draw(|frame| {
                let _ = paint_attached(
                    frame,
                    state,
                    conversation,
                    &tab.streaming,
                    &tab.view_state,
                    &mut render_state,
                    &HashMap::new(),
                    crate::domain::models::PermissionMode::Normal,
                    &crate::adapters::tui::state::AttachInfo {
                        read_only: false,
                        channel_count: 1,
                    },
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[tokio::test]
    async fn busy_and_context_actions_keep_independent_ownership_across_modal_and_reopen() {
        use crate::adapters::daemon::protocol::{
            RecipientAction, RecipientActionOutcome, RecipientActionTarget,
        };
        use crate::adapters::tui::handlers::recipient_actions::{
            action_is_pending, apply_action_result, surface_action_notices,
        };
        use crate::adapters::tui::presentation::{self, Surface};
        use crate::adapters::tui::state::{RecipientActionState, ThreadAccess, ThreadTarget};
        use crate::domain::models::SemanticMessageType;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut rig = attach_keys::tests::Rig::new(false);
        let workspace = rig.workspace().to_path_buf();
        let journal =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(&workspace)
                .await
                .unwrap();
        let address = append_received_test_item(
            &journal,
            "ri_busy_context",
            SemanticMessageType::Consultation,
            "A real journal-backed consultation body.",
        )
        .await;
        rig.state.input_buffer = "unsent local composition".to_owned();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        let open = async |rig: &mut attach_keys::tests::Rig| {
            crate::infrastructure::runtime::thread_open::open_received(
                &workspace,
                Some(&rig.domain_tab.conversation),
                address.item().as_str(),
                &mut rig.state,
                ThreadAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
        };
        open(&mut rig).await;
        let target = RecipientActionTarget::Item {
            item_id: address.item().as_str().to_owned(),
        };
        press_surface_key(&mut rig, 'b').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::DeferRecipientItem { item_id }]
                if item_id == address.item().as_str()
        ));
        assert!(action_is_pending(
            &rig.state,
            RecipientAction::Defer,
            &ThreadTarget::Item(address.clone()),
            None,
        ));
        rig.raw_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(rig.state.thread_inspector.is_none());
        assert!(!rig.state.feedback_blocks.contains_key("recipient-actions"));
        assert_eq!(rig.state.recipient_actions.len(), 1);
        assert_eq!(rig.state.input_buffer, "unsent local composition");

        open(&mut rig).await;
        assert!(!paint_received_surface(&mut rig.state).contains("peer-tainted"));
        let notices_before_modal = surface_action_notices(
            &rig.state.recipient_actions,
            &ThreadTarget::Item(address.clone()),
            None,
        )
        .count();
        // A modal owner above the thread frame keeps the key from it (the registry resolves
        // against the TOP frame only).
        presentation::reducer::open(&mut rig.state, Surface::Help);
        assert!(presentation::is_open(&rig.state, Surface::Help));
        press_surface_key(&mut rig, 'u').await;
        assert!(
            rig.frames().is_empty(),
            "modal-owned Context key cannot send a mutation"
        );
        assert!(matches!(
            &rig.state.thread_inspector.as_ref().unwrap().target,
            ThreadTarget::Item(opened) if opened == &address
        ));
        assert_eq!(rig.state.input_buffer, "unsent local composition");
        assert_eq!(
            surface_action_notices(
                &rig.state.recipient_actions,
                &ThreadTarget::Item(address.clone()),
                None,
            )
            .count(),
            notices_before_modal,
            "the modal-owned key leaves the busy notice intact"
        );
        // Positive control: once the modal closes the thread frame takes the same key.
        rig.raw_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(!presentation::is_open(&rig.state, Surface::Help));
        assert!(presentation::is_open(&rig.state, Surface::ThreadInspector));
        press_surface_key(&mut rig, 'u').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::SetRecipientContext {
                item_id,
                selected: true,
            }] if item_id == address.item().as_str()
        ));
        assert!(action_is_pending(
            &rig.state,
            RecipientAction::Defer,
            &ThreadTarget::Item(address.clone()),
            None,
        ));
        assert!(action_is_pending(
            &rig.state,
            RecipientAction::ContextSelect,
            &ThreadTarget::Item(address.clone()),
            None,
        ));
        assert_eq!(rig.state.recipient_actions.len(), 2);
        assert_eq!(rig.state.input_buffer, "unsent local composition");

        apply_action_result(
            &mut rig.state,
            RecipientAction::ContextSelect,
            target.clone(),
            RecipientActionOutcome::ContextSelection {
                selected: true,
                changed: true,
            },
        );
        assert!(
            rig.state
                .recipient_context_ids
                .contains(address.item().as_str())
        );
        let surface = rig.state.thread_inspector.as_ref().unwrap();
        assert!(surface.context_selected);
        assert!(
            paint_received_surface(&mut rig.state).contains("peer-tainted"),
            "actual cached header must repaint after correlated selection"
        );
        assert_eq!(rig.state.input_buffer, "unsent local composition");
        assert!(action_is_pending(
            &rig.state,
            RecipientAction::Defer,
            &ThreadTarget::Item(address.clone()),
            None,
        ));

        press_surface_key(&mut rig, 'u').await;
        assert!(matches!(
            rig.frames().as_slice(),
            [ClientFrame::SetRecipientContext {
                item_id,
                selected: false,
            }] if item_id == address.item().as_str()
        ));
        apply_action_result(
            &mut rig.state,
            RecipientAction::ContextRemove,
            target,
            RecipientActionOutcome::ContextSelection {
                selected: false,
                changed: true,
            },
        );
        assert!(
            !rig.state
                .recipient_context_ids
                .contains(address.item().as_str())
        );
        let surface = rig.state.thread_inspector.as_ref().unwrap();
        assert!(!surface.context_selected);
        assert!(
            !paint_received_surface(&mut rig.state).contains("peer-tainted"),
            "actual cached header must repaint after correlated removal"
        );
        assert_eq!(rig.state.input_buffer, "unsent local composition");
        assert!(action_is_pending(
            &rig.state,
            RecipientAction::Defer,
            &ThreadTarget::Item(address.clone()),
            None,
        ));
        assert!(matches!(
            rig.state.recipient_actions.get(&(
                RecipientAction::Defer,
                RecipientActionTarget::Item {
                    item_id: address.item().as_str().to_owned(),
                },
            )),
            Some(RecipientActionState::Pending { .. })
        ));

        rig.raw_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        open(&mut rig).await;
        assert!(action_is_pending(
            &rig.state,
            RecipientAction::Defer,
            &ThreadTarget::Item(address),
            None,
        ));
        assert_eq!(rig.state.input_buffer, "unsent local composition");
    }

    #[tokio::test]
    async fn operator_commands_reach_their_daemon_hooks() {
        use crossterm::event::KeyCode;
        let (tx, mut rx) = mpsc::unbounded_channel();
        for command in ["/team defer ri_explicit", "/clear"] {
            let mut state = TuiState::new(80, 24);
            state.input_buffer = command.to_owned();
            state.cursor_position = state.input_buffer.chars().count();
            let outcome = attached_composer_key(KeyCode::Enter, false, &mut state, &tx);
            if let ComposerOutcome::Submit(text) = outcome {
                assert_eq!(text, command, "the daemon receives the complete command");
                assert!(
                    rx.try_recv().is_err(),
                    "one command cannot also queue a second action"
                );
            } else {
                match (
                    command,
                    rx.try_recv().expect("command reaches its daemon hook"),
                ) {
                    ("/team defer ri_explicit", ClientFrame::DeferRecipientItem { item_id }) => {
                        assert_eq!(item_id, "ri_explicit")
                    }
                    ("/clear", ClientFrame::UserMessage { text, images }) => {
                        assert_eq!(text, "/clear");
                        assert!(images.is_empty());
                    }
                    (_, frame) => panic!("command reached the wrong daemon action: {frame:?}"),
                }
            }
        }
    }

    #[test]
    fn story21_6_attached_recipient_classification_uses_host_controls() {
        use crate::adapters::daemon::response_modes::{
            AWAITING_RESPONSE_PLACEHOLDER, DRAFT_APPROVAL_PREFIX, DRAFTING_PLACEHOLDER,
        };
        let conversation = Conversation {
            messages: vec![
                peer_row("peer-response-node-a", "already sent"),
                peer_row("peer-response-node-b", AWAITING_RESPONSE_PLACEHOLDER),
                peer_row(
                    "peer-response-node-c",
                    &format!("{DRAFT_APPROVAL_PREFIX}draft body"),
                ),
                peer_row("peer-response-node-d", DRAFTING_PLACEHOLDER),
                peer_row("peer-response-node-e", AWAITING_RESPONSE_PLACEHOLDER),
            ],
            peer_row_controls: [
                (
                    "peer-response-node-a".to_owned(),
                    peer_control(
                        "node-a",
                        crate::domain::models::PeerRowStage::AwaitingOperator,
                    ),
                ),
                (
                    "peer-response-node-b".to_owned(),
                    peer_control("node-b", crate::domain::models::PeerRowStage::Drafting),
                ),
                (
                    "peer-response-node-c".to_owned(),
                    peer_control("node-c", crate::domain::models::PeerRowStage::Ready),
                ),
                (
                    "peer-response-node-d".to_owned(),
                    peer_control("node-d", crate::domain::models::PeerRowStage::Settled),
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        use crate::adapters::tui::decision_queue::{self, DecisionSubject, RecipientDraftStage};
        let mut state = TuiState::new(80, 24);
        decision_queue::sync_recipient_drafts(&mut state, &conversation, false);
        let items = state.decisions.items().collect::<Vec<_>>();
        assert_eq!(
            items.len(),
            2,
            "drafting and settled rows do not require a decision"
        );
        assert!(
            matches!(&items[0].subject, DecisionSubject::RecipientDraft(p)
            if p.node == "node-a" && p.stage == RecipientDraftStage::AwaitingOperator)
        );
        assert!(
            matches!(&items[1].subject, DecisionSubject::RecipientDraft(p)
            if p.node == "node-c" && p.stage == RecipientDraftStage::Ready("draft body".into()))
        );
    }

    #[test]
    fn story21_6_attached_no_pending_rows_means_no_recipient_item() {
        let conversation = Conversation {
            messages: vec![peer_row("peer-response-node-d", "already sent")],
            ..Default::default()
        };
        let mut state = TuiState::new(80, 24);
        crate::adapters::tui::decision_queue::sync_recipient_drafts(
            &mut state,
            &conversation,
            false,
        );
        assert_eq!(state.decisions.len(), 0);
    }

    #[test]
    fn retract_target_is_the_latest_unretracted_automatic_row() {
        let mut latest = peer_row("peer-response-node-b", "second auto reply");
        latest.authorship = crate::domain::models::Authorship::AgentComposed;
        latest.send_authorization = crate::domain::models::SendAuthorization::Automatic;
        let mut older = peer_row("peer-response-node-a", "first auto reply");
        older.authorship = crate::domain::models::Authorship::AgentComposed;
        older.send_authorization = crate::domain::models::SendAuthorization::Automatic;
        older.retracted_at_ms = Some(123);
        let conversation = Conversation {
            messages: vec![peer_row("human-1", "operator text"), older, latest.clone()],
            ..Default::default()
        };
        let target = retractable_rows(&conversation)
            .next()
            .cloned()
            .expect("a retract target");
        assert_eq!(target.id, latest.id);
        // …and once it is retracted, no target remains.
        let conversation = Conversation {
            messages: vec![{
                let mut row = target;
                row.retracted_at_ms = Some(456);
                row
            }],
            ..Default::default()
        };
        assert!(retractable_rows(&conversation).next().is_none());
    }

    #[test]
    fn story19_19_review_tool_evidence_has_no_retraction_authority() {
        let mut response = peer_row("peer-response-node-ready", "automatic response");
        response.authorship = crate::domain::models::Authorship::AgentComposed;
        response.send_authorization = crate::domain::models::SendAuthorization::Automatic;
        let mut evidence = peer_row("peer-tool-evidence-generated", "tool execution evidence");
        evidence.authorship = crate::domain::models::Authorship::AgentComposed;
        let conversation = Conversation {
            messages: vec![response.clone(), evidence.clone()],
            ..Default::default()
        };
        assert_eq!(
            retractable_rows(&conversation)
                .next()
                .map(|row| row.id.clone()),
            Some(response.id),
            "displayed peer tool evidence cannot mask an actual response target"
        );
        let conversation = Conversation {
            messages: vec![evidence],
            ..Default::default()
        };
        assert!(retractable_rows(&conversation).next().is_none());
    }

    #[test]
    fn retractable_rows_skip_human_decided_rows() {
        use crate::domain::models::{Authorship, PeerRowStage, SendAuthorization};
        let mut automatic = peer_row("peer-response-automatic", "automatic");
        automatic.authorship = Authorship::Unknown;
        automatic.send_authorization = SendAuthorization::Automatic;
        let mut approved = peer_row("peer-response-approved", "approved");
        approved.authorship = Authorship::AgentComposed;
        approved.send_authorization = SendAuthorization::HumanDecided;
        let mut evidence = peer_row("peer-tool-evidence-generated", "evidence");
        evidence.authorship = Authorship::AgentComposed;
        let mut rap = peer_row("generated-rap-auto", "unsent RAP template");
        rap.authorship = Authorship::AgentComposed;
        let mut pending = peer_row("peer-response-pending", "pending");
        pending.send_authorization = SendAuthorization::Automatic;
        let mut conversation = Conversation {
            messages: vec![automatic.clone(), approved, evidence, rap, pending],
            peer_row_controls: [(
                "peer-response-pending".to_owned(),
                peer_control("pending", PeerRowStage::Ready),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert_eq!(
            retractable_rows(&conversation)
                .next()
                .map(|row| row.id.clone()),
            Some(automatic.id),
            "M07: finder must skip newer human-decided, evidence, RAP and unsettled rows"
        );
        conversation.messages.remove(0);
        assert!(
            retractable_rows(&conversation).next().is_none(),
            "without the automatic row, no remaining row has retraction authority"
        );
    }

    #[test]
    fn preview_collapses_newlines_and_bounds_length() {
        assert_eq!(preview_line("short", 10), "short");
        assert_eq!(preview_line("line one\nline two", 80), "line one line two");
        let bounded = preview_line(&"x".repeat(200), 10);
        assert_eq!(bounded.chars().count(), 10);
        assert!(bounded.ends_with('…'));
    }

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
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        }
    }

    pub(super) fn snapshot_with(transcript: Vec<ChatMessage>, blocked: usize) -> AttachSnapshot {
        AttachSnapshot {
            conversation_id: "conv-test".into(),
            transcript,
            permission_mode: PermissionMode::Normal,
            channels: vec![ChannelKind::Terminal],
            blocked_actions_waiting: blocked,
            pending_consent_cards: 0,
            retained_approvals: 0,
            peer_row_controls: BTreeMap::new(),
            peer_row_revision: 0,
            recipient_context_ids: Vec::new(),
            withheld_tool_inputs: Vec::new(),
        }
    }

    fn provider(chunk: StreamChunk) -> RawEvent {
        RawEvent {
            conversation_id: Some("conv-test".into()),
            timestamp_ms: 0,
            kind: RawEventKind::Provider(chunk),
        }
    }

    fn paint_attached_tool_inputs(
        conversation: &Conversation,
        streaming: &StreamingState,
        inputs: &AttachedToolInputs,
    ) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 24)).unwrap();
        let theme = crate::adapters::tui::theme::Theme::dark();
        let mut tab = TabRenderState::default();
        let domain_tab =
            crate::domain::models::tab::TabState::from_conversation(0, conversation.clone());
        terminal
            .draw(|frame| {
                chat_pane::render_attached(
                    frame,
                    frame.area(),
                    &domain_tab.conversation,
                    streaming,
                    &domain_tab.view_state,
                    &theme,
                    &mut tab,
                    &BTreeMap::new(),
                    &inputs.reasons,
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn withheld_input_notice_tracks_provider_part_and_committed_render_ids() {
        use crate::adapters::daemon::protocol::ToolInputWithheld;

        for peer in [false, true] {
            for transition_reason in [false, true] {
                for reason in [
                    ToolInputWithheld::PeerTier,
                    ToolInputWithheld::TooLarge {
                        bytes: 9 * 1024 * 1024,
                    },
                ] {
                    let clock = SystemClock::default();
                    let mut conversation = Conversation::default();
                    let mut streaming = StreamingState::default();
                    let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
                    let mut state = TuiState::new(180, 24);
                    let mut mode = PermissionMode::Normal;
                    let mut inputs = AttachedToolInputs::default();
                    let wrap = |chunk| RawEvent {
                        conversation_id: Some("conv-test".into()),
                        timestamp_ms: 0,
                        kind: if peer {
                            RawEventKind::PeerProvider(chunk)
                        } else {
                            RawEventKind::Provider(chunk)
                        },
                    };
                    let event = wrap(StreamChunk::ToolUse {
                        id: "provider-wire-id".into(),
                        name: "Write".into(),
                        input: serde_json::Value::Null,
                    });
                    // Exercise the omitted-input wire form, not a fabricated map.
                    let (event, withheld) = if transition_reason {
                        (event, None)
                    } else {
                        let frame = DaemonFrame::ToolEventWithheld {
                            event,
                            withheld: reason,
                        };
                        let decoded: DaemonFrame =
                            serde_json::from_slice(&serde_json::to_vec(&frame).unwrap()).unwrap();
                        match decoded {
                            DaemonFrame::ToolEventWithheld { event, withheld } => {
                                (event, Some(withheld))
                            }
                            _ => unreachable!(),
                        }
                    };
                    inputs.apply(
                        event,
                        withheld,
                        &mut conversation,
                        &mut streaming,
                        &mut reducer,
                        &mut state,
                        &mut mode,
                        &clock,
                    );
                    let turn = reducer.open_turn.as_ref().unwrap();
                    let render_id =
                        tool_call_id_for(&turn.id, reducer.pending_invocations["provider-wire-id"]);
                    assert_ne!(render_id, "provider-wire-id");
                    inputs.apply(
                        wrap(StreamChunk::ToolResult {
                            id: "provider-wire-id".into(),
                            content: "written".into(),
                            is_error: false,
                            operator_private: false,
                            diff: crate::domain::models::WriteDiffState::NotAWrite,
                            images: Vec::new(),
                        }),
                        None,
                        &mut conversation,
                        &mut streaming,
                        &mut reducer,
                        &mut state,
                        &mut mode,
                        &clock,
                    );
                    if transition_reason {
                        inputs.apply(
                            RawEvent {
                                conversation_id: Some("conv-test".into()),
                                timestamp_ms: 0,
                                kind: RawEventKind::Tool(
                                    crate::domain::models::ToolCallTransition {
                                        conversation_id: "conv-test".into(),
                                        call: ToolCall::Executing {
                                            id: "provider-wire-id".into(),
                                            request: crate::domain::models::ToolCallRequest {
                                                id: "provider-wire-id".into(),
                                                tool_name: "Write".into(),
                                                input: serde_json::Value::Null,
                                            },
                                            started_at: 42,
                                        },
                                    },
                                ),
                            },
                            Some(reason),
                            &mut conversation,
                            &mut streaming,
                            &mut reducer,
                            &mut state,
                            &mut mode,
                            &clock,
                        );
                    }
                    assert_eq!(inputs.reasons.get(&render_id), Some(&reason));
                    inputs.apply(
                        wrap(StreamChunk::TurnComplete {
                            stop_reason: StopReason::EndTurn,
                        }),
                        None,
                        &mut conversation,
                        &mut streaming,
                        &mut reducer,
                        &mut state,
                        &mut mode,
                        &clock,
                    );
                    if !peer {
                        let committed =
                            paint_attached_tool_inputs(&conversation, &streaming, &inputs);
                        assert!(committed.contains(&reason.notice()), "{committed}");
                        assert_eq!(
                            conversation.messages.last().unwrap().tool_calls[0].id,
                            render_id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn reused_provider_id_preserves_genuine_null_without_stale_withholding() {
        use crate::adapters::daemon::protocol::ToolInputWithheld;

        let clock = SystemClock::default();
        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut state = TuiState::new(180, 24);
        let mut mode = PermissionMode::Normal;
        let mut inputs = AttachedToolInputs::default();
        for withheld in [Some(ToolInputWithheld::PeerTier), None] {
            inputs.apply(
                provider(StreamChunk::ToolUse {
                    id: "reused-wire-id".into(),
                    name: "Write".into(),
                    input: serde_json::Value::Null,
                }),
                withheld,
                &mut conversation,
                &mut streaming,
                &mut reducer,
                &mut state,
                &mut mode,
                &clock,
            );
            inputs.apply(
                provider(StreamChunk::ToolResult {
                    id: "reused-wire-id".into(),
                    content: "written".into(),
                    is_error: false,
                    operator_private: false,
                    diff: crate::domain::models::WriteDiffState::NotAWrite,
                    images: Vec::new(),
                }),
                None,
                &mut conversation,
                &mut streaming,
                &mut reducer,
                &mut state,
                &mut mode,
                &clock,
            );
            inputs.apply(
                provider(StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                }),
                None,
                &mut conversation,
                &mut streaming,
                &mut reducer,
                &mut state,
                &mut mode,
                &clock,
            );
            let latest = conversation.messages.last().unwrap().clone();
            let tool = &latest.tool_calls[0];
            assert_eq!(tool.input, serde_json::Value::Null);
            assert_eq!(inputs.reasons.get(&tool.id).copied(), withheld);
            let only_latest = Conversation {
                messages: vec![latest],
                ..Default::default()
            };
            let painted = paint_attached_tool_inputs(&only_latest, &streaming, &inputs);
            if let Some(reason) = withheld {
                assert!(painted.contains(&reason.notice()), "{painted}");
            } else {
                assert!(!painted.contains("input withheld"), "{painted}");
                assert!(!painted.contains("input too large"), "{painted}");
                assert!(painted.contains("Write"), "{painted}");
            }
        }
        assert_eq!(conversation.messages.len(), 2);
        assert_ne!(
            conversation.messages[0].tool_calls[0].id,
            conversation.messages[1].tool_calls[0].id
        );
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
            let _ = read_frame::<_, ClientFrame>(&mut sr).await.unwrap();
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
        let mut lifecycle =
            crate::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle::default();
        let stale_cancel = CancellationToken::new();
        let (bus, _events) = mpsc::unbounded_channel();
        lifecycle.register(
            conversation.id.clone(),
            stale_cancel.clone(),
            "stale-local".into(),
            bus,
        );
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
            lifecycle: &mut lifecycle,
            session_manager: &mut session_manager,
        };
        driver.submit(sub, view).await;

        assert!(
            !lifecycle.is_live(&conversation.id),
            "no local producer (row 15)"
        );
        assert!(
            stale_cancel.is_cancelled(),
            "stale local ownership is retired"
        );
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
        let mut lifecycle =
            crate::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle::default();
        let stale_cancel = CancellationToken::new();
        let (bus, _events) = mpsc::unbounded_channel();
        lifecycle.register(
            conversation.id.clone(),
            stale_cancel.clone(),
            "stale-local".into(),
            bus,
        );
        let mut session_manager = SessionManager::new(SessionState::Empty);

        let view = TurnViewState {
            conversation: &mut conversation,
            streaming: &mut streaming,
            state: &mut state,
            lifecycle: &mut lifecycle,
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
        assert!(!lifecycle.is_live(&conversation.id), "no local producer");
        assert!(
            stale_cancel.is_cancelled(),
            "closed socket also retires stale local ownership"
        );
        assert!(
            state.notifications.records().any(|record| {
                record.source == NoticeSource::Attached
                    && record.severity == NoticeSeverity::Error
                    && record.message.contains("not sent")
            }),
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
        let mut state = TuiState::new(80, 24);
        state.status = StatusState::Streaming;
        let mut mode = PermissionMode::Normal;

        apply_client_event(
            provider(StreamChunk::Text {
                content: "hello world".into(),
                parent_tool_use_id: None,
            }),
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut state,
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
            &mut state,
            &mut mode,
            &clock,
        );
        assert_eq!(state.status, StatusState::Idle);
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
        let mut state = TuiState::new(80, 24);
        state.status = StatusState::Streaming;
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
            &mut state,
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
        let mut state = TuiState::new(80, 24);
        state.status = StatusState::Idle;
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
            &mut state,
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
            &mut state,
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
            &mut state,
            &mut mode,
            &clock,
        );
        assert_eq!(conversation.messages.last().unwrap().content, "after");
    }

    #[test]
    fn forwarded_room_event_reaches_attached_tui_feedback() {
        let clock = SystemClock::default();
        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut state = TuiState::new(80, 24);
        let mut mode = PermissionMode::Normal;

        let redraw = apply_client_event(
            RawEvent {
                conversation_id: None,
                timestamp_ms: 0,
                kind: RawEventKind::DomainEvent(crate::domain::events::DomainEventPayload::Room(
                    crate::domain::models::RoomEvent::RemoteEnvelopeRejected {
                        rail: None,
                        peer: crate::domain::models::PeerId::from_public_key(&[7; 32])
                            .expect("valid test peer"),
                        reason: crate::domain::models::RejectReason::Policy {
                            detail: "blocked by policy".to_owned(),
                        },
                        task: Some("remote-task-7".to_owned()),
                        direction: crate::domain::models::Direction::Inbound,
                    },
                )),
            },
            &mut conversation,
            &mut streaming,
            &mut reducer,
            &mut state,
            &mut mode,
            &clock,
        );

        assert!(
            redraw,
            "a forwarded room refusal changes attached TUI feedback"
        );
        let block = state
            .feedback_blocks
            .values()
            .next()
            .expect("attached TUI renders a transparency feedback block");
        assert!(
            block.message.contains("blocked by policy"),
            "{}",
            block.message
        );
        assert!(block.id.contains("remote-task-7"), "{}", block.id);
    }

    /// An intercepted `/team` verb is never submitted to the model. Every
    /// parsed command has an explicit local presentation or daemon-frame route.
    ///
    /// Mutant → RED: delete the `Remove` arm. The line classifies as
    /// `PassThrough` and the operator's removal is typed at the model.
    #[test]
    fn an_attached_team_remove_becomes_a_frame_not_a_model_prompt() {
        match attached_team_line("/team remove ri_x") {
            AttachedTeamLine::Frame(ClientFrame::RemoveRecipientItem { item_id }) => {
                assert_eq!(item_id, "ri_x");
            }
            other => panic!("`/team remove` must be intercepted as a frame, got {other:?}"),
        }
        // The shipped verb is unchanged…
        assert!(matches!(
            attached_team_line("/team ack ri_x"),
            AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { .. })
        ));
        // …a malformed removal is consumed and explained, never submitted…
        let AttachedTeamLine::Refused(error) = attached_team_line("/team remove") else {
            panic!("a malformed removal must never become a model turn");
        };
        assert!(error.contains("/team remove"), "{error}");
        assert!(matches!(
            attached_team_line("/team remove ri_x extra"),
            AttachedTeamLine::Refused(_)
        ));
        // …Story 19.16g: `/team log` is now a LOCAL read, never a model turn…
        assert!(matches!(
            attached_team_line("  /team log --json"),
            AttachedTeamLine::LocalLog(args) if args.json && args.filter.is_none()
        ));
        // …and the pre-existing fall-through is preserved exactly.
        assert!(matches!(
            attached_team_line("tell me about /team remove"),
            AttachedTeamLine::PassThrough
        ));
    }

    #[test]
    fn attached_team_classifier_covers_every_command_and_board_selector() {
        use crate::adapters::daemon::protocol::TeamBoardRequest;

        let cases = [
            ("/team", "log"),
            (" /team", "log"),
            ("/team\tlog --json", "log"),
            ("/team log --json", "log"),
            ("/team board", "board"),
            ("/team board peer-a", "board-peer"),
            ("/team board --last", "board-last"),
            ("/team board --action act-1", "board-action"),
            ("/team board --actions", "board-actions"),
            (
                "/team send --type story_assignment --subject sprint-7 a,b hello",
                "send",
            ),
            ("/team ack ri-a", "ack"),
            ("/team remove ri-a", "remove"),
            ("/team retract a ri-a", "retract"),
            ("/team reply ri-a hello", "reply"),
            ("/team defer ri-a", "defer"),
            ("/team view ri-a", "view"),
            ("/team context", "context"),
            ("/team context ri-a", "context-select"),
            ("/team context off ri-a", "context-off"),
            ("/team thread a ri-a", "thread"),
            ("/team followup a turn-1 hello", "followup"),
            ("/team trust", "refused"),
            ("/team trust x", "refused"),
            ("/team untrust a", "untrust"),
            ("/team status", "status"),
        ];
        for (line, expected) in cases {
            let classified = attached_team_line_for_request(line, "req-1".to_owned());
            let actual = match classified {
                AttachedTeamLine::Refused(_) => "refused",
                AttachedTeamLine::LocalLog(_) => "log",
                AttachedTeamLine::Frame(ClientFrame::TeamBoard {
                    selector: TeamBoardRequest::Inventory,
                    ..
                }) => "board",
                AttachedTeamLine::Frame(ClientFrame::TeamBoard {
                    selector: TeamBoardRequest::Peer(_),
                    ..
                }) => "board-peer",
                AttachedTeamLine::Frame(ClientFrame::TeamBoard {
                    selector: TeamBoardRequest::Last,
                    ..
                }) => "board-last",
                AttachedTeamLine::Frame(ClientFrame::TeamBoard {
                    selector: TeamBoardRequest::Action(_),
                    ..
                }) => "board-action",
                AttachedTeamLine::Frame(ClientFrame::TeamBoard {
                    selector: TeamBoardRequest::Actions,
                    ..
                }) => "board-actions",
                AttachedTeamLine::Frame(ClientFrame::TeamSend { .. }) => "send",
                AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { .. }) => "ack",
                AttachedTeamLine::Frame(ClientFrame::RemoveRecipientItem { .. }) => "remove",
                AttachedTeamLine::Frame(ClientFrame::TeamRetractPreview { .. }) => "retract",
                AttachedTeamLine::Frame(ClientFrame::ReplyToRecipientItem { .. }) => "reply",
                AttachedTeamLine::Frame(ClientFrame::DeferRecipientItem { .. }) => "defer",
                AttachedTeamLine::LocalView(_) => "view",
                AttachedTeamLine::Frame(ClientFrame::InspectRecipientContext) => "context",
                AttachedTeamLine::Frame(ClientFrame::SetRecipientContext {
                    selected: true,
                    ..
                }) => "context-select",
                AttachedTeamLine::Frame(ClientFrame::SetRecipientContext {
                    selected: false,
                    ..
                }) => "context-off",
                AttachedTeamLine::Frame(ClientFrame::TeamThread { .. }) => "thread",
                AttachedTeamLine::Frame(ClientFrame::TeamFollowup { .. }) => "followup",
                AttachedTeamLine::Frame(ClientFrame::TeamStatus { .. }) => "status",
                AttachedTeamLine::Frame(ClientFrame::RevokeSenderConsent { .. }) => "untrust",
                other => panic!("{line}: unexpected classification {other:?}"),
            };
            assert_eq!(actual, expected, "{line}");
        }
        assert!(matches!(
            attached_team_line("/teamwork"),
            AttachedTeamLine::PassThrough
        ));
        assert!(matches!(
            attached_team_line("plain text"),
            AttachedTeamLine::PassThrough
        ));
    }

    #[test]
    fn attached_trust_refusal_sends_no_frame_or_provider_input() {
        use crate::adapters::tui::handlers::team_command::trust_refusal;
        use crossterm::event::KeyCode;

        for read_only in [false, true] {
            for line in ["/team trust", "/team trust x"] {
                assert!(matches!(
                    attached_team_line(line),
                    AttachedTeamLine::Refused(message) if message == trust_refusal()
                ));
                let mut state = TuiState::new(80, 24);
                let (tx, mut rx) = mpsc::unbounded_channel();
                let mut requests = AttachedTeamRequests::default();
                state.input_buffer = line.to_owned();
                state.cursor_position = state.input_buffer.chars().count();
                assert!(matches!(
                    attached_composer_key_with_team_requests(
                        KeyCode::Enter,
                        read_only,
                        &mut state,
                        &tx,
                        &mut requests,
                    ),
                    ComposerOutcome::Handled
                ));
                assert!(state.input_buffer.is_empty());
                assert!(matches!(
                    rx.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
            }
        }
    }

    #[test]
    fn story19_23_resolved_aliases_start_attached_rows_before_settlements() {
        let mut state = TuiState::new(80, 24);
        let mut requests = AttachedTeamRequests::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let AttachedTeamLine::Frame(frame) =
            attached_team_line_for_request("/team send --holders-of S1,S2 body", "selected".into())
        else {
            panic!("selector command");
        };
        assert!(enqueue_attached_team_frame(
            &mut state,
            &mut requests,
            frame,
            &tx
        ));
        assert!(
            matches!(rx.try_recv().unwrap(),ClientFrame::TeamSend {recipients,holders_of:Some(_),..} if recipients.is_empty())
        );
        let raw = serde_json::json!({"teamSendStarted":{
            "request":"selected","action":"sa_selected","recipients":["jun","aisha"]
        }});
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            serde_json::from_value(raw.clone()).unwrap(),
            false,
            &tx,
        );
        let AttachedTeamRequest::Send { block_id, .. } = &requests.pending["selected"] else {
            panic!("send");
        };
        let block_id = block_id.clone();
        assert_eq!(
            state.team_send_blocks[&block_id]
                .iter()
                .map(|row| row.alias.as_str())
                .collect::<Vec<_>>(),
            ["jun", "aisha"]
        );
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamSendSettled {
                request: "selected".into(),
                index: 0,
                outcome: crate::adapters::daemon::protocol::TeamSendOutcome::Delivered,
            },
            false,
            &tx,
        );
        assert_eq!(
            state.team_send_blocks[&block_id][0].outcome,
            Some(crate::domain::models::RecipientOutcome::Delivered)
        );
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            serde_json::from_value(raw).unwrap(),
            false,
            &tx,
        );
        assert_eq!(
            state.team_send_blocks[&block_id][0].outcome,
            Some(crate::domain::models::RecipientOutcome::Delivered)
        );
    }

    #[test]
    fn story19_23_unrecorded_selected_send_settles_not_sent_without_rows() {
        let mut state = TuiState::new(80, 24);
        let mut requests = AttachedTeamRequests::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let AttachedTeamLine::Frame(frame) =
            attached_team_line_for_request("/team send --holders-of S1,S2 body", "selected".into())
        else {
            panic!("selector command");
        };
        assert!(enqueue_attached_team_frame(
            &mut state,
            &mut requests,
            frame,
            &tx
        ));
        let AttachedTeamRequest::Send { block_id, .. } = &requests.pending["selected"] else {
            panic!("send");
        };
        let block_id = block_id.clone();
        // The daemon's action record failed: settlements arrive with no start.
        for index in 0..2 {
            apply_attached_team_frame(
                &mut state,
                &mut requests,
                DaemonFrame::TeamSendSettled {
                    request: "selected".into(),
                    index,
                    outcome: crate::adapters::daemon::protocol::TeamSendOutcome::NotSent,
                },
                false,
                &tx,
            );
        }
        assert!(!requests.pending.contains_key("selected"));
        assert!(!state.team_send_blocks.contains_key(&block_id));
        assert_eq!(
            state.feedback_blocks[&block_id].message,
            format!(
                "  {}",
                crate::domain::models::team_send::TEAM_SEND_NOT_SENT_LABEL
            )
        );
    }

    #[test]
    fn attached_read_commands_precede_read_only_guard_and_mutations_do_not() {
        use crossterm::event::KeyCode;
        let mut state = TuiState::new(80, 24);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut requests = AttachedTeamRequests::default();
        for command in ["/team board --last", "/team status"] {
            state.input_buffer = command.to_owned();
            state.cursor_position = state.input_buffer.chars().count();
            assert!(matches!(
                attached_composer_key_with_team_requests(
                    KeyCode::Enter,
                    true,
                    &mut state,
                    &tx,
                    &mut requests,
                ),
                ComposerOutcome::FrameSent
            ));
            assert!(matches!(
                rx.try_recv(),
                Ok(ClientFrame::TeamBoard { .. } | ClientFrame::TeamStatus { .. })
            ));
            assert!(state.input_buffer.is_empty());
        }
        for command in ["/team send a hello", "/team untrust a"] {
            state.input_buffer = command.to_owned();
            state.cursor_position = state.input_buffer.chars().count();
            assert!(matches!(
                attached_composer_key_with_team_requests(
                    KeyCode::Enter,
                    true,
                    &mut state,
                    &tx,
                    &mut requests,
                ),
                ComposerOutcome::Handled
            ));
            assert_eq!(state.input_buffer, command);
            assert!(rx.try_recv().is_err());
        }
        let draft = "ordinary read-only draft";
        state.input_buffer = draft.to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        assert!(matches!(
            attached_composer_key_with_team_requests(
                KeyCode::Enter,
                true,
                &mut state,
                &tx,
                &mut requests,
            ),
            ComposerOutcome::Handled
        ));
        assert_eq!(
            state.input_buffer, draft,
            "read-only submission retains the draft"
        );
        assert!(rx.try_recv().is_err(), "read-only draft is never forwarded");
    }

    #[test]
    fn attached_paste_keeps_multiline_team_send_body_intact() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer =
            "/team send --type story_assignment --subject sprint-7 jun-dev ".to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        let body = "line one\n  indented line\n\nfinal line";
        attach_keys::handle_attached_paste(&mut state, body, false);
        let AttachedTeamLine::Frame(ClientFrame::TeamSend {
            request,
            recipients,
            text,
            message_type,
            subject,
            stories: None,
            holders_of: None,
        }) = attached_team_line_for_request(&state.input_buffer, "paste-1".into())
        else {
            panic!("pasted send command must stay a typed daemon frame");
        };
        assert_eq!(request, "paste-1");
        assert_eq!(recipients, ["jun-dev"]);
        assert_eq!(text, body);
        assert_eq!(
            message_type,
            Some(crate::domain::models::SemanticMessageType::StoryAssignment)
        );
        assert_eq!(subject.as_deref(), Some("sprint-7"));
    }

    #[test]
    fn attached_team_send_enqueue_failure_paints_only_its_block() {
        let mut state = TuiState::new(80, 24);
        crate::adapters::tui::handlers::team_command::show_team_status(
            &mut state,
            "prior status".into(),
        );
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let mut requests = AttachedTeamRequests::default();
        assert!(!enqueue_attached_team_frame(
            &mut state,
            &mut requests,
            ClientFrame::TeamSend {
                request: "failed-send".into(),
                recipients: vec!["a".into()],
                text: "body".into(),
                message_type: None,
                subject: None,
                stories: None,
                holders_of: None,
            },
            &tx,
        ));
        assert!(
            state.feedback_blocks["team-send-1"]
                .message
                .contains("could not enqueue")
        );
        assert_eq!(state.feedback_blocks["team-status"].message, "prior status");
        assert!(state.team_send_blocks.is_empty());
        assert!(requests.pending.is_empty());
    }

    #[test]
    fn attached_requests_correlate_interleaved_sends_and_discard_stale_boards() {
        use crate::adapters::daemon::protocol::{
            ProtocolError, RecipientAction, RecipientActionOutcome, TeamBoardRequest,
        };
        use crate::domain::models::FeedbackLevel;
        let mut state = TuiState::new(80, 24);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut requests = AttachedTeamRequests::default();
        for (request, alias) in [("r1", "a"), ("r2", "b")] {
            assert!(enqueue_attached_team_frame(
                &mut state,
                &mut requests,
                ClientFrame::TeamSend {
                    request: request.into(),
                    recipients: vec![alias.into()],
                    text: "body".into(),
                    message_type: None,
                    subject: None,
                    stories: None,
                    holders_of: None,
                },
                &tx,
            ));
            let _ = rx.try_recv().unwrap();
        }
        let untouched = state.feedback_blocks["team-send-1"].message.clone();
        apply_attached_team_refusal(
            &mut state,
            &mut requests,
            RecipientAction::TeamBoard,
            "r1".into(),
            RecipientActionOutcome::Refused {
                error: ProtocolError::Internal("wrong kind".into()),
            },
        );
        assert_eq!(state.feedback_blocks["team-send-1"].message, untouched);
        assert!(requests.pending.contains_key("r1"));
        for request in ["r1", "r2"] {
            apply_attached_team_frame(
                &mut state,
                &mut requests,
                DaemonFrame::TeamSendStarted {
                    recipients: Vec::new(),
                    request: request.into(),
                    action: crate::domain::models::SendActionId::mint(),
                },
                false,
                &mpsc::unbounded_channel().0,
            );
        }
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamSendSettled {
                request: "r1".into(),
                index: 0,
                outcome: crate::adapters::daemon::protocol::TeamSendOutcome::Delivered,
            },
            false,
            &mpsc::unbounded_channel().0,
        );
        apply_attached_team_refusal(
            &mut state,
            &mut requests,
            RecipientAction::TeamSend,
            "r2".into(),
            RecipientActionOutcome::Refused {
                error: ProtocolError::Internal("refused r2".into()),
            },
        );
        assert!(
            state.feedback_blocks["team-send-1"]
                .message
                .contains("delivered")
        );
        assert!(
            state.feedback_blocks["team-send-2"]
                .message
                .contains("refused r2")
        );
        assert!(state.team_send_blocks.is_empty());
        assert!(requests.pending.is_empty());

        requests.pending.insert(
            "board-old".into(),
            AttachedTeamRequest::Board {
                next_sequence: 0,
                view: String::new(),
            },
        );
        requests.pending.insert(
            "board-new".into(),
            AttachedTeamRequest::Board {
                next_sequence: 0,
                view: String::new(),
            },
        );
        requests.latest_board = Some("board-new".into());
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamViewChunk {
                request: "board-old".into(),
                sequence: 0,
                view: "old view".into(),
            },
            false,
            &mpsc::unbounded_channel().0,
        );
        assert!(!state.feedback_blocks.contains_key("team-board"));
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamViewChunk {
                request: "board-new".into(),
                sequence: 0,
                view: "new view".into(),
            },
            false,
            &mpsc::unbounded_channel().0,
        );
        assert!(
            !state.feedback_blocks.contains_key("team-board"),
            "a partial board is never painted"
        );
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamViewDone {
                request: "board-new".into(),
                chunks: 1,
            },
            false,
            &mpsc::unbounded_channel().0,
        );
        assert_eq!(state.feedback_blocks["team-board"].message, "new view");
        assert_eq!(
            state.feedback_blocks["team-board"].level,
            FeedbackLevel::Info
        );
        requests.pending.insert(
            "board-invalid".into(),
            AttachedTeamRequest::Board {
                next_sequence: 0,
                view: String::new(),
            },
        );
        requests.latest_board = Some("board-invalid".into());
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamViewChunk {
                request: "board-invalid".into(),
                sequence: 1,
                view: "invalid partial".into(),
            },
            false,
            &mpsc::unbounded_channel().0,
        );
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamViewDone {
                request: "board-invalid".into(),
                chunks: 1,
            },
            false,
            &mpsc::unbounded_channel().0,
        );
        assert_eq!(
            state.feedback_blocks["team-board"].message, "new view",
            "out-of-order chunks discard the entire response"
        );
        let _ = TeamBoardRequest::Actions;
    }
    /// `DF-19-16C` review bullet — a pasted leading space must not smuggle a
    /// destructive `/team` verb past the intercept table into the model.
    ///
    /// Mutant → RED: classify the raw buffer again (drop `trim_start`).
    #[test]
    fn a_leading_space_no_longer_turns_an_intercepted_team_verb_into_a_model_turn() {
        assert!(
            matches!(
                attached_team_line("  /team remove ri_x"),
                AttachedTeamLine::Frame(ClientFrame::RemoveRecipientItem { .. })
            ),
            "a pasted leading space used to fail `strip_prefix`, classify \
             PassThrough, and submit the removal to the model as a turn"
        );
        assert!(matches!(
            attached_team_line("\t/team ack ri_x"),
            AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { .. })
        ));
        // Control: trimming the LEADING whitespace must not start matching
        // `/team` mid-sentence.
        assert!(matches!(
            attached_team_line("please run /team remove ri_x"),
            AttachedTeamLine::PassThrough
        ));
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
        let domain_tab = crate::domain::models::tab::TabState::from_conversation(0, conv);
        let mut trs = TabRenderState::default();
        let fbs = BTreeMap::new();
        term.draw(|f| {
            chat_pane::render_attached(
                f,
                f.area(),
                &domain_tab.conversation,
                &domain_tab.streaming,
                &domain_tab.view_state,
                &theme,
                &mut trs,
                &fbs,
                &HashMap::new(),
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
    /// on attach so the user doesn't have to scroll to discover them (AC8). Story
    /// 21.1 AC6: it is a store notice, never a transcript row.
    #[test]
    fn blocked_actions_waiting_surfaces_summary() {
        let snap = snapshot_with(vec![user_msg("a", ChannelKind::Terminal)], 3);
        let conv = seed_conversation(&snap);
        assert!(
            !conv
                .messages
                .iter()
                .any(|m| m.content.contains("waiting on you")),
            "the summary is never a transcript row"
        );
        let notice = blocked_actions_notice(&snap).expect("blocked-action summary missing");
        let mut state = TuiState::new(80, 24);
        notifications::warning(&mut state, NoticeSource::Attached, notice);
        assert!(
            state.notifications.visible().any(|record| {
                record.severity == NoticeSeverity::Warning
                    && record.message.contains("3 action(s) waiting on you")
            }),
            "blocked-action summary missing from the first frame's toasts"
        );

        // None waiting → no summary.
        let snap0 = snapshot_with(vec![user_msg("a", ChannelKind::Terminal)], 0);
        assert!(
            blocked_actions_notice(&snap0).is_none(),
            "summary must not show when nothing is waiting"
        );
    }

    /// AC7 client half — a daemon-emitted purge-notice `SystemNotice` becomes a store
    /// notice through the normal `RawEvent` path (no new frame); Story 21.1 AC6 maps
    /// its wire level onto the store severity and never writes a transcript row.
    #[test]
    fn purge_notice_system_notice_renders_inline() {
        let clock = SystemClock::default();
        let mut conversation = Conversation::default();
        let mut streaming = StreamingState::default();
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut state = TuiState::new(80, 24);
        state.status = StatusState::Idle;
        let mut mode = PermissionMode::Normal;

        for (level, severity, message) in [
            (
                NoticeLevel::Info,
                NoticeSeverity::Info,
                "5 facts removed from MEMORY.md — purged from search index",
            ),
            (NoticeLevel::Warning, NoticeSeverity::Warning, "warn notice"),
            (
                NoticeLevel::Advisory,
                NoticeSeverity::Warning,
                "advisory notice",
            ),
            (NoticeLevel::Error, NoticeSeverity::Error, "error notice"),
        ] {
            apply_client_event(
                RawEvent {
                    conversation_id: None,
                    timestamp_ms: 0,
                    kind: RawEventKind::SystemNotice {
                        level,
                        message: message.into(),
                    },
                },
                &mut conversation,
                &mut streaming,
                &mut reducer,
                &mut state,
                &mut mode,
                &clock,
            );
            assert!(
                state.notifications.records().any(|record| {
                    record.source == NoticeSource::Daemon
                        && record.severity == severity
                        && record.message == message
                }),
                "{message}: SystemNotice must become a {severity:?} store notice"
            );
        }
        assert!(
            conversation.messages.is_empty(),
            "a SystemNotice is never a transcript row"
        );
    }

    /// K9 — the daemon notice frames reach the store through the helper the
    /// frame loop itself calls; none is a transcript row and the wire is
    /// unchanged (`PROTOCOL_VERSION == 16`, no frame shape touched).
    #[test]
    fn story21_1_attached_notice_frames_are_store_notices_and_the_wire_is_unchanged() {
        use crate::adapters::daemon::protocol::{
            InterruptOutcome, InterruptRefusal, PROTOCOL_VERSION, TurnGeneration,
        };
        assert_eq!(PROTOCOL_VERSION, 16, "no attach-protocol change in 21-1");
        let mut state = TuiState::new(80, 24);
        let mut interrupt =
            crate::infrastructure::runtime::attach_interrupt::AttachedInterrupt::new(false);
        let conversation = Conversation::default();
        let frames = [
            (
                DaemonFrame::FrameTooLarge {
                    frame: "History".into(),
                    bytes: 5_000_000,
                },
                Some((NoticeSource::Daemon, NoticeSeverity::Warning, "History")),
            ),
            (
                DaemonFrame::Error(ProtocolError::ReadOnly),
                Some((NoticeSource::Daemon, NoticeSeverity::Warning, "read-only")),
            ),
            (
                DaemonFrame::Error(ProtocolError::Internal("boom".into())),
                Some((
                    NoticeSource::Daemon,
                    NoticeSeverity::Error,
                    "[daemon error]",
                )),
            ),
            (
                DaemonFrame::ActiveTurn {
                    generation: Some(TurnGeneration(7)),
                },
                None,
            ),
            (
                DaemonFrame::InterruptResult {
                    generation: TurnGeneration(7),
                    outcome: InterruptOutcome::Refused {
                        reason: InterruptRefusal::NoActiveTurn,
                    },
                },
                Some((
                    NoticeSource::Attached,
                    NoticeSeverity::Warning,
                    "stop refused: no turn is running",
                )),
            ),
        ];
        let mut expected = 0;
        for (frame, notice) in &frames {
            apply_notice_frame(&mut state, &mut interrupt, frame);
            if let Some((source, severity, fragment)) = notice {
                expected += 1;
                let newest = state.notifications.records().last().expect("a record");
                assert_eq!(
                    (newest.source, newest.severity),
                    (*source, *severity),
                    "{frame:?}"
                );
                assert!(newest.message.contains(fragment), "{}", newest.message);
            }
            assert_eq!(state.notifications.len(), expected, "{frame:?}");
        }
        assert_eq!(
            state.notifications.unseen(),
            4,
            "every warning/error occurrence counts toward the badge"
        );
        assert!(conversation.messages.is_empty(), "never a transcript row");
    }

    /// K9 — the attached tick and draw expire toasts on the injected clock:
    /// the tick asks for a redraw only once a deadline has passed, and the
    /// first frame drawn at or after a deadline no longer paints that toast
    /// (the frame loop's `begin_draw` + shared compose, without a socket).
    #[test]
    fn story21_1_attached_tick_and_draw_expire_toasts_on_the_injected_clock() {
        use crate::adapters::tui::notification_log::LogFacts;
        use crate::adapters::tui::widgets::toast_layer;
        use crate::domain::clock::MockClock;
        use crate::domain::models::NotificationsConfig;

        fn painted(state: &mut TuiState) -> String {
            use ratatui::layout::Rect;
            let (width, height) = (100u16, 24u16);
            notifications::begin_draw(state);
            let chat_pane = Rect::new(0, 0, width, height - 5);
            state
                .notifications
                .set_capacity(toast_layer::capacity_for(chat_pane.height));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let state = &*state;
            terminal
                .draw(|f| {
                    let layout = crate::adapters::tui::layout::compute_layout(
                        f.area(),
                        &state.theme,
                        "",
                        1,
                        false,
                        DensityMode::Focus,
                    )
                    .unwrap();
                    let shell = crate::adapters::tui::presentation::Shell::new(f.area(), &layout);
                    crate::adapters::tui::presentation::compose(
                        f,
                        state,
                        &shell,
                        crate::adapters::tui::keymap::Host::Attached,
                        &LogFacts::default(),
                        &mut crate::adapters::tui::presentation::occlusion::OpaqueRects::new(
                            shell.chat_pane,
                        ),
                        |_| {},
                    );
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        }

        let clock = std::sync::Arc::new(MockClock::new(Instant::now()));
        let mut state = TuiState::new(100, 24);
        state.notifications =
            NotificationStore::new(clock.clone(), NotificationsConfig::default().clamped());
        let mut interrupt =
            crate::infrastructure::runtime::attach_interrupt::AttachedInterrupt::new(false);
        notifications::info(&mut state, NoticeSource::Attached, "info-marker");
        apply_notice_frame(
            &mut state,
            &mut interrupt,
            &DaemonFrame::Error(ProtocolError::ReadOnly),
        );
        apply_notice_frame(
            &mut state,
            &mut interrupt,
            &DaemonFrame::Error(ProtocolError::Internal("error-marker".into())),
        );
        let first = painted(&mut state);
        assert!(first.contains("info-marker") && first.contains("read-only"));
        assert!(first.contains("error-marker"));

        // 1 ms before the Info deadline (4 s): the tick stays quiet and the frame keeps it.
        clock.advance(Duration::from_secs(4) - Duration::from_millis(1));
        state.needs_redraw = false;
        notifications::tick(&mut state);
        assert!(!state.needs_redraw, "no deadline has passed yet");
        assert!(painted(&mut state).contains("info-marker"));

        // At the deadline the tick asks for a redraw and the frame drops the Info.
        clock.advance(Duration::from_millis(1));
        notifications::tick(&mut state);
        assert!(state.needs_redraw, "the deadline passed: redraw");
        let frame = painted(&mut state);
        assert!(!frame.contains("info-marker"), "{frame}");
        assert!(frame.contains("read-only") && frame.contains("error-marker"));

        // Warning at 10 s, Error at 60 s; the records stay in the log.
        clock.advance(Duration::from_secs(6));
        let frame = painted(&mut state);
        assert!(!frame.contains("read-only") && frame.contains("error-marker"));
        clock.advance(Duration::from_secs(50));
        let frame = painted(&mut state);
        assert!(!frame.contains("error-marker"), "{frame}");
        assert_eq!(state.notifications.len(), 3, "S-LOG keeps every record");
    }

    fn assert_attached_auth_commands(names: &[&str], source: NoticeSource, message: &str) {
        use crossterm::event::KeyCode;

        for read_only in [false, true] {
            for name in names {
                let valid = [
                    format!("/{name}"),
                    format!(" /{name} openai "),
                    format!("/{name}\topenai"),
                    format!("/{name}\u{2003}openai"),
                    format!("/{name} openai"),
                    format!("/{name} anthropic"),
                    format!(" /{name} "),
                ];
                let invalid = [
                    format!("/ {name} openai"),
                    format!("/\t{name} openai"),
                    format!("/\n{name} openai"),
                    format!("/\u{2003}{name} openai"),
                    format!("/{name}x"),
                    format!("/{name}-extra"),
                    "/Logout".into(),
                    "say /logout".into(),
                ];
                for (lines, accepted) in [(&valid[..], true), (&invalid[..], false)] {
                    for line in lines {
                        let context = format!("input: {line:?}, read_only: {read_only}");
                        let mut state = TuiState::new(80, 24);
                        state.needs_redraw = false;
                        let (tx, mut rx) = mpsc::unbounded_channel();
                        state.input_buffer = line.clone();
                        state.cursor_position = state.input_buffer.chars().count();
                        let outcome =
                            attached_composer_key(KeyCode::Enter, read_only, &mut state, &tx);
                        assert!(
                            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                            "{context}: zero frames"
                        );
                        if accepted {
                            assert!(matches!(outcome, ComposerOutcome::Handled), "{context}");
                            assert!(state.input_buffer.is_empty(), "{context}");
                            assert!(state.feedback_blocks.is_empty(), "{context}");
                            assert_eq!(state.notifications.len(), 1, "{context}");
                            let record = state.notifications.records().next().unwrap();
                            assert_eq!(record.source, source, "{context}");
                            assert_eq!(record.message, message, "{context}");
                            assert_eq!(record.severity, NoticeSeverity::Info, "{context}");
                            assert!(record.action.is_none(), "{context}");
                            assert!(state.needs_redraw, "{context}");
                        } else {
                            assert!(state.feedback_blocks.is_empty(), "{context}");
                            if read_only {
                                assert!(matches!(outcome, ComposerOutcome::Handled), "{context}");
                                assert_eq!(state.input_buffer, *line, "{context}");
                                assert_eq!(state.notifications.len(), 1, "{context}");
                                let record = state.notifications.records().next().unwrap();
                                assert_eq!(
                                    record.message, "read-only — can't send here",
                                    "{context}"
                                );
                                assert_eq!(record.severity, NoticeSeverity::Info, "{context}");
                                assert!(state.needs_redraw, "{context}");
                            } else {
                                assert!(
                                    matches!(outcome, ComposerOutcome::Submit(text) if text == *line),
                                    "{context}"
                                );
                                assert!(state.input_buffer.is_empty(), "{context}");
                                assert!(state.notifications.is_empty(), "{context}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// Attached sign-in commands answer locally; malformed names follow the literal rail.
    #[test]
    fn attached_sign_in_commands_are_answered_locally() {
        assert_attached_auth_commands(
            &["login", "models"],
            NoticeSource::Login,
            "Sign-in is not available in an attached session. Run `rustain auth login <provider>` in a terminal; restart the daemon to unlock newly signed-in providers.",
        );
    }

    /// Account commands use the same grammar for writable and read-only composers.
    #[test]
    fn attached_account_commands_are_answered_locally() {
        assert_attached_auth_commands(
            &["logout", "accounts"],
            NoticeSource::Accounts,
            "Account changes are not available in an attached session. Run `rustain auth accounts`, `rustain auth use <provider> --id <N>` or `rustain auth logout <provider> --id <N>` in a terminal.",
        );
    }

    /// Story 19.16g K08 / M08 — the attached `/team log` is a LOCAL read
    /// (zero frames, never a model turn), presented only by a successful
    /// draw; its read-only path remains reachable before the composer guard,
    /// and the observer never touches a turn.
    #[tokio::test]
    async fn attached_log_is_local_and_streaming_survives() {
        use crate::adapters::tui::state::LogAwareness;
        use crate::infrastructure::transparency_awareness::LogAwarenessObserver;
        use crate::infrastructure::transparency_awareness::test_support::append_rows;
        use crossterm::event::KeyCode;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        fn draw_attached(state: &mut TuiState) -> String {
            let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let mut tab = TabRenderState::default();
            let domain_tab =
                crate::domain::models::tab::TabState::from_conversation(0, Conversation::default());
            term.draw(|f| {
                paint_attached(
                    f,
                    state,
                    &domain_tab.conversation,
                    &domain_tab.streaming,
                    &domain_tab.view_state,
                    &mut tab,
                    &HashMap::new(),
                    crate::domain::models::PermissionMode::Normal,
                    &crate::adapters::tui::state::AttachInfo {
                        read_only: false,
                        channel_count: 1,
                    },
                );
            })
            .unwrap();
            attached_screen(&term)
        }

        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 3).await;
        let service = crate::infrastructure::transparency::TransparencyService::new(
            std::sync::Arc::new(
                crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                    workspace.path(),
                ),
            ),
            workspace.path().to_path_buf(),
        );
        let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
        let mut state = TuiState::new(100, 30);
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientFrame>();
        async fn poll(observer: &mut LogAwarenessObserver, state: &mut TuiState) -> LogAwareness {
            // Every call is a later scheduled observation.
            static SECONDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let second = SECONDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            observer.tick_at(
                Instant::now() + Duration::from_secs(3_600 * second),
                &mut state.log_awareness,
            );
            observer.settle(&mut state.log_awareness).await;
            state.log_awareness.display
        }
        assert_eq!(
            poll(&mut observer, &mut state).await,
            LogAwareness::Unseen(3)
        );

        // A leading-space `/team log` on the writable rail: local, no frame.
        state.input_buffer = "  /team log".to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        let ComposerOutcome::LocalLog(args) =
            attached_composer_key(KeyCode::Enter, false, &mut state, &tx)
        else {
            panic!("`/team log` must be a local read on the attached rail");
        };
        assert!(state.input_buffer.is_empty());
        let notices = crate::infrastructure::runtime::transparency_bridge::attached_team_log(
            &service, &mut state, &args,
        )
        .await;
        assert!(notices.is_empty(), "{notices:?}");
        assert!(
            rx.try_recv().is_err(),
            "zero frames: never a model UserMessage"
        );
        assert!(
            state
                .thread_inspector
                .as_ref()
                .unwrap()
                .content
                .contains("append-only")
        );

        // Painted, but the draw failed: nothing presented.
        let screen = draw_attached(&mut state);
        assert!(screen.contains("A2A log"), "{screen}");
        after_attached_draw(&mut state, false);
        assert_eq!(
            poll(&mut observer, &mut state).await,
            LogAwareness::Unseen(3)
        );
        assert!(draw_attached(&mut state).contains("A2A log"));
        after_attached_draw(&mut state, true);
        assert_eq!(poll(&mut observer, &mut state).await, LogAwareness::Hidden);

        // An incoming retract row and a poll never touch a streaming turn.
        state.status = StatusState::Streaming;
        append_rows(workspace.path(), 1).await;
        assert_eq!(
            poll(&mut observer, &mut state).await,
            LogAwareness::Unseen(1)
        );
        assert!(matches!(state.status, StatusState::Streaming));

        // A local log error renders as a plain local line, never a
        // turn-fatal notice and never a frame.
        state.input_buffer = "/team log --filter=bogus=term".to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        let ComposerOutcome::LocalLog(args) =
            attached_composer_key(KeyCode::Enter, false, &mut state, &tx)
        else {
            panic!("a parseable log line is local");
        };
        let notices = crate::infrastructure::runtime::transparency_bridge::attached_team_log(
            &service, &mut state, &args,
        )
        .await;
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(matches!(state.status, StatusState::Streaming));

        // Read-only: the composer edits and the local log works…
        state.input_buffer = String::new();
        state.cursor_position = state.input_buffer.chars().count();
        for c in "/team logx".chars() {
            attached_composer_key(KeyCode::Char(c), true, &mut state, &tx);
        }
        attached_composer_key(KeyCode::Backspace, true, &mut state, &tx);
        assert_eq!(state.input_buffer, "/team log");
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, true, &mut state, &tx),
            ComposerOutcome::LocalLog(_)
        ));
        // Read-only attachments may inspect inventory, but item-argument
        // context frames remain writer-only.
        state.input_buffer = "/team context".to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, true, &mut state, &tx),
            ComposerOutcome::FrameSent
        ));
        assert!(state.input_buffer.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientFrame::InspectRecipientContext)
        ));
        for line in ["/team context ri_x", "/team context off ri_x"] {
            state.input_buffer = line.to_owned();
            state.cursor_position = state.input_buffer.chars().count();
            assert!(matches!(
                attached_composer_key(KeyCode::Enter, true, &mut state, &tx),
                ComposerOutcome::Handled
            ));
            assert_eq!(state.input_buffer, line);
            assert!(
                state
                    .notifications
                    .records()
                    .next_back()
                    .is_some_and(|record| {
                        record.severity == NoticeSeverity::Info
                            && record.message == "read-only — can't send here"
                    })
            );
        }
        assert!(rx.try_recv().is_err(), "item context writes are refused");

        // Writable text still submits; an intercepted verb still frames.
        state.input_buffer = "hello model".to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, false, &mut state, &tx),
            ComposerOutcome::Submit(text) if text == "hello model"
        ));
        state.input_buffer = "/team ack ri_x".to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, false, &mut state, &tx),
            ComposerOutcome::FrameSent
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientFrame::AcknowledgeRecipientItem { .. })
        ));
    }

    #[test]
    fn reply_remains_a_frame_and_deferred_verb_classifier_is_explicit() {
        match attached_team_line("/team reply ri_x hi  there") {
            AttachedTeamLine::Frame(ClientFrame::ReplyToRecipientItem {
                item_id,
                text,
                verdict,
            }) => {
                assert_eq!(item_id, "ri_x");
                assert_eq!(text, "hi  there");
                assert_eq!(verdict, None);
            }
            other => panic!("expected the reply frame, got {other:?}"),
        }
        // Story 19.22 `AC4(d)` — verdict forms classify to the same frame.
        for (line, verdict, body) in [
            ("/team reply --accept ri_x", "accept", ""),
            (
                "/team reply --accept ri_x fine by me",
                "accept",
                "fine by me",
            ),
            (
                "/team reply --discuss ri_x worried about scope",
                "discuss",
                "worried about scope",
            ),
        ] {
            match attached_team_line(line) {
                AttachedTeamLine::Frame(ClientFrame::ReplyToRecipientItem {
                    item_id,
                    text,
                    verdict: actual,
                }) => {
                    assert_eq!(item_id, "ri_x", "{line}");
                    assert_eq!(
                        actual,
                        Some(if verdict == "accept" {
                            crate::adapters::daemon::protocol::ReplyVerdict::Accept
                        } else {
                            crate::adapters::daemon::protocol::ReplyVerdict::Discuss
                        }),
                        "{line}"
                    );
                    assert_eq!(text, body, "{line}");
                }
                other => panic!("expected the verdict reply frame for {line}, got {other:?}"),
            }
        }
        // A flag after the id is body: a plain Answer, never PassThrough.
        match attached_team_line("/team reply ri_x --accept fine") {
            AttachedTeamLine::Frame(ClientFrame::ReplyToRecipientItem {
                text, verdict, ..
            }) => {
                assert_eq!(text, "--accept fine");
                assert_eq!(verdict, None);
            }
            other => panic!("expected a plain Answer frame, got {other:?}"),
        }
        for line in [
            "/team thread b ri_x",
            "/team followup b rr_y hi",
            "/team retract b ri_x",
        ] {
            assert!(matches!(
                attached_team_line(line),
                AttachedTeamLine::Frame(_)
            ));
        }
        assert!(matches!(
            attached_team_line("/teamwork"),
            AttachedTeamLine::PassThrough
        ));
        assert!(matches!(
            attached_team_line("/team ack ri_x"),
            AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { .. })
        ));
    }
    #[test]
    fn local_view_does_not_select_recipient_context() {
        use crossterm::event::KeyCode;

        let mut state = TuiState::new(80, 24);
        state.input_buffer = "/team view ri_view_only".to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, false, &mut state, &tx),
            ComposerOutcome::LocalView(item_id) if item_id == "ri_view_only"
        ));
        assert!(rx.try_recv().is_err(), "View is local display, not consent");
    }
    // Replaces `scoped_surface_ctrl_c_and_ctrl_d_take_attached_detach_exit`
    // (Story 21.2 AC8/AC13): Ctrl+C/Ctrl+D used to be the surface's detach exit;
    // RU-3/RU-20 make Ctrl+C "back one frame" and Ctrl+D inert — never a detach.
    #[tokio::test]
    async fn story21_2_scoped_surface_ctrl_c_closes_and_ctrl_d_is_inert_never_detach() {
        use crate::adapters::tui::presentation::{self, Surface};
        use crate::adapters::tui::state::ThreadAccess;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut rig = attach_keys::tests::Rig::new(false);
        let workspace = rig.workspace().to_path_buf();
        let journal =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(&workspace)
                .await
                .unwrap();
        let address = append_received_test_item(
            &journal,
            "ri_detach_surface",
            crate::domain::models::SemanticMessageType::Consultation,
            "A received item for scoped detach control.",
        )
        .await;
        crate::infrastructure::runtime::thread_open::open_received(
            &workspace,
            Some(&rig.domain_tab.conversation),
            address.item().as_str(),
            &mut rig.state,
            ThreadAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        rig.state.input_buffer = "preserved local input".to_owned();
        rig.state.cursor_position = rig.state.input_buffer.chars().count();
        let key = |chord| KeyEvent::new(KeyCode::Char(chord), KeyModifiers::CONTROL);
        assert_eq!(
            rig.raw_key(key('d')).await,
            attach_keys::KeyFlow::Continue,
            "Ctrl+D never detaches"
        );
        assert!(
            presentation::is_open(&rig.state, Surface::ThreadInspector),
            "Ctrl+D is inert for the frame (a viewer scroll, never a close)"
        );
        assert_eq!(
            rig.raw_key(key('c')).await,
            attach_keys::KeyFlow::Continue,
            "Ctrl+C never detaches"
        );
        assert!(
            rig.state.thread_inspector.is_none()
                && !presentation::is_open(&rig.state, Surface::ThreadInspector),
            "Ctrl+C is back one frame: it closes the surface like Esc"
        );
        assert_eq!(rig.state.input_buffer, "preserved local input");
        assert!(rig.frames().is_empty(), "neither chord is a peer action");
    }
}

#[cfg(test)]
mod attached_parity_tests {
    use super::*;
    use crate::adapters::daemon::protocol::{TeamRetractPreviewView, TeamSendOutcome};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn submit(
        state: &mut TuiState,
        requests: &mut AttachedTeamRequests,
        line: &str,
        reader: bool,
        tx: &mpsc::UnboundedSender<ClientFrame>,
    ) -> ComposerOutcome {
        state.input_buffer = line.to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        attached_composer_key_with_team_requests(KeyCode::Enter, reader, state, tx, requests)
    }
    fn preview(
        state: &mut TuiState,
        requests: &mut AttachedTeamRequests,
        armed: bool,
        busy: bool,
        tx: &mpsc::UnboundedSender<ClientFrame>,
    ) {
        requests
            .pending
            .insert("retract".into(), AttachedTeamRequest::RetractPreview);
        apply_attached_team_frame(
            state,
            requests,
            DaemonFrame::TeamRetractPreviewed {
                request: "retract".into(),
                preview: TeamRetractPreviewView::Card {
                    peer: "B".into(),
                    item_id: "ri_a".into(),
                    body: "Exact card body".into(),
                    armed,
                },
            },
            busy,
            tx,
        );
    }

    /// 6D-L1-03: an unrelated deferred confirmation must not decline a new
    /// daemon slot, but an outstanding slot must be explicitly released.
    #[test]
    fn story21_6_deferred_consolidation_allows_preview_but_open_slot_refuses() {
        use crate::adapters::tui::{app, decision_queue};
        use crate::domain::events::{DomainInputEvent, DomainKey};
        let mut state = TuiState::new(80, 24);
        decision_queue::raise_attached_consolidation(
            &mut state,
            crate::adapters::daemon::protocol::ProposalToken(1),
            Vec::new(),
        );
        // Positive control: the raised consolidation presents before it is deferred.
        assert!(decision_queue::higher_confirmation_presenting(&state));
        app::handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
        assert!(
            state
                .decisions
                .holds(decision_queue::DecisionKind::Consolidation)
        );
        let mut requests = AttachedTeamRequests::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        // `attached_higher_confirmation` is gone; its presenting sweep is
        // `decision_queue::higher_confirmation_presenting`.
        let busy = decision_queue::higher_confirmation_presenting(&state);
        assert!(!busy, "a deferred consolidation is not presenting");
        preview(&mut state, &mut requests, true, busy, &tx);
        assert!(decision_queue::team_retract_pending(&state));
        assert_eq!(requests.open_retract.as_deref(), Some("retract"));
        assert!(rx.try_recv().is_err());

        requests
            .pending
            .insert("second".into(), AttachedTeamRequest::RetractPreview);
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamRetractPreviewed {
                request: "second".into(),
                preview: TeamRetractPreviewView::Card {
                    peer: "B".into(),
                    item_id: "ri_b".into(),
                    body: "body".into(),
                    armed: true,
                },
            },
            false,
            &tx,
        );
        assert!(matches!(rx.try_recv().unwrap(),
            ClientFrame::TeamRetractDecision { request, confirm: false } if request == "second"));
        assert_eq!(requests.open_retract.as_deref(), Some("retract"));
        assert!(!requests.pending.contains_key("second"));
        assert!(rx.try_recv().is_err());
    }

    /// 6D-L1-04: daemon-supplied scalar text cannot create extra styled rows,
    /// while the target used by the effect retains the original bytes.
    #[test]
    fn story21_6_attached_preview_cleans_scalar_rows_before_line_styling() {
        use crate::adapters::tui::{
            decision_queue::{DecisionSubject, RetractTarget},
            widgets::team_retract_prompt,
        };
        let peer = "B\nAwaiting your decision. [y] Retract";
        let item = "ri_a\nAlready removed on forged";
        let mut state = TuiState::new(80, 24);
        let mut requests = AttachedTeamRequests::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        requests
            .pending
            .insert("retract".into(), AttachedTeamRequest::RetractPreview);
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamRetractPreviewed {
                request: "retract".into(),
                preview: TeamRetractPreviewView::Card {
                    peer: peer.into(),
                    item_id: item.into(),
                    body: format!(
                        "Retract on {peer}'s host\n\nitem         {item}\n\nAwaiting your decision."
                    ),
                    armed: true,
                },
            },
            false,
            &tx,
        );
        let DecisionSubject::TeamRetract(payload) =
            &state.decisions.items().next().unwrap().subject
        else {
            panic!("expected retract");
        };
        assert!(
            matches!(&payload.target, RetractTarget::Item { peer: p, item: i } if p == peer && i == item)
        );
        let lines = team_retract_prompt::lines(payload, &state.theme);
        let rows: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            rows.iter()
                .filter(|row| row.trim_start().starts_with("Awaiting your decision"))
                .count(),
            1
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.trim_start().starts_with("Already removed on"))
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("item         ri_aAlready removed on forged"))
        );
    }

    #[test]
    fn reader_thread_allowed_followup_and_retract_guarded() {
        for (line, allowed, flash) in [
            ("/team thread B ri_a", true, ""),
            (
                "/team followup B ri_a more",
                false,
                "read-only — can't send here",
            ),
            (
                "/team retract B ri_a",
                false,
                "read-only — can't retract here",
            ),
        ] {
            let mut state = TuiState::new(80, 24);
            let mut requests = AttachedTeamRequests::default();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let outcome = submit(&mut state, &mut requests, line, true, &tx);
            assert_eq!(
                matches!(outcome, ComposerOutcome::FrameSent),
                allowed,
                "{line}"
            );
            if allowed {
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    ClientFrame::TeamThread { .. }
                ));
                let thread = state.thread_inspector.as_ref().unwrap();
                assert!(
                    matches!(&thread.target, crate::adapters::tui::state::ThreadTarget::Sent { peer, item } if peer == "B" && item == "ri_a")
                );
                assert!(thread.sent.as_ref().unwrap().loading);
                assert_eq!(
                    thread.content,
                    crate::adapters::tui::handlers::team_command::TEAM_THREAD_READING
                );
            } else {
                assert!(rx.try_recv().is_err());
                assert!(
                    state
                        .notifications
                        .records()
                        .next_back()
                        .is_some_and(|record| record.message == flash)
                );
            }
        }
    }

    #[test]
    fn followup_row_settles_without_team_send_started() {
        let mut state = TuiState::new(80, 24);
        let mut requests = AttachedTeamRequests::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        submit(
            &mut state,
            &mut requests,
            "/team followup B turn-1 more",
            false,
            &tx,
        );
        let ClientFrame::TeamFollowup {
            request,
            peer,
            reply_to,
            text,
        } = rx.try_recv().unwrap()
        else {
            panic!("linked frame")
        };
        assert_eq!(
            (peer.as_str(), reply_to.as_str(), text.as_str()),
            ("B", "turn-1", "more")
        );
        assert!(
            state.feedback_blocks["team-send-1"]
                .message
                .contains("sending…")
        );
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamSendSettled {
                request,
                index: 0,
                outcome: TeamSendOutcome::Delivered,
            },
            false,
            &tx,
        );
        assert_eq!(
            state.feedback_blocks["team-send-1"].message,
            "Send · ● B delivered"
        );
        assert!(requests.pending.is_empty());
    }

    #[tokio::test]
    async fn story21_6_attached_retract_key_matrix_and_double_answer() {
        use crate::adapters::tui::decision_queue::{self, DecisionSubject};
        for armed in [false, true] {
            for key in [
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ] {
                let mut rig = attach_keys::tests::Rig::new(false);
                preview(
                    &mut rig.state,
                    &mut rig.team_requests,
                    armed,
                    false,
                    &rig.tx,
                );
                let item = rig.state.decisions.items().next().unwrap();
                assert!(
                    matches!(&item.subject, DecisionSubject::TeamRetract(payload)
                    if payload.card == "Exact card body" && payload.armed == armed)
                );
                rig.ready();
                rig.raw_key(key).await;
                let expected = match key.code {
                    KeyCode::Char('y') if armed => Some(true),
                    KeyCode::Char('n') => Some(false),
                    _ => None,
                };
                if let Some(confirm) = expected {
                    assert!(matches!(rig.frames().as_slice(),
                        [ClientFrame::TeamRetractDecision { request, confirm: found }]
                            if request == "retract" && *found == confirm));
                    assert!(!decision_queue::team_retract_pending(&rig.state));
                    assert!(rig.team_requests.open_retract.is_none());
                    if confirm {
                        assert!(matches!(
                            rig.team_requests.pending["retract"],
                            AttachedTeamRequest::Retract { .. }
                        ));
                    } else {
                        assert!(rig.team_requests.pending.is_empty());
                    }
                    rig.raw_key(key).await;
                } else {
                    assert!(decision_queue::team_retract_pending(&rig.state));
                    assert_eq!(rig.team_requests.open_retract.as_deref(), Some("retract"));
                }
                assert!(rig.frames().is_empty(), "{key:?} must not send twice");
            }
        }
    }

    #[test]
    fn story21_6_attached_preview_busy_declines_and_second_retract_never_replaces_item() {
        for busy in [false, true] {
            let mut state = TuiState::new(80, 24);
            let mut requests = AttachedTeamRequests::default();
            let (tx, mut rx) = mpsc::unbounded_channel();
            preview(&mut state, &mut requests, true, busy, &tx);
            if busy {
                assert!(!crate::adapters::tui::decision_queue::team_retract_pending(
                    &state
                ));
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    ClientFrame::TeamRetractDecision { request, confirm: false } if request == "retract"
                ));
                assert!(
                    state.feedback_blocks["team-retract"]
                        .message
                        .starts_with("Another confirmation")
                );
                assert!(rx.try_recv().is_err());
            } else {
                submit(
                    &mut state,
                    &mut requests,
                    "/team retract B ri_other",
                    false,
                    &tx,
                );
                assert!(rx.try_recv().is_err());
                assert!(matches!(
                    &state.decisions.items().next().unwrap().subject,
                    crate::adapters::tui::decision_queue::DecisionSubject::TeamRetract(payload)
                        if matches!(&payload.target, crate::adapters::tui::decision_queue::RetractTarget::Item { item, .. } if item == "ri_a")
                ));
            }
        }
        let mut state = TuiState::new(80, 24);
        let mut requests = AttachedTeamRequests::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        submit(
            &mut state,
            &mut requests,
            "/team retract B ri_a",
            false,
            &tx,
        );
        rx.try_recv().unwrap();
        submit(
            &mut state,
            &mut requests,
            "/team retract B ri_other",
            false,
            &tx,
        );
        assert!(rx.try_recv().is_err());
        assert!(
            state.feedback_blocks["team-retract"]
                .message
                .starts_with("A retract is already")
        );
    }

    #[test]
    fn thread_latest_only_and_retract_done_keeps_answer_pending() {
        let mut state = TuiState::new(80, 24);
        let mut requests = AttachedTeamRequests::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut keys = Vec::new();
        for _ in 0..2 {
            submit(&mut state, &mut requests, "/team thread B ri_a", false, &tx);
            let ClientFrame::TeamThread { request, .. } = rx.try_recv().unwrap() else {
                panic!("thread")
            };
            keys.push(request);
        }
        for (key, view) in [(keys[0].clone(), "stale"), (keys[1].clone(), "latest")] {
            apply_attached_team_frame(
                &mut state,
                &mut requests,
                DaemonFrame::TeamViewChunk {
                    request: key.clone(),
                    sequence: 0,
                    view: view.into(),
                },
                false,
                &tx,
            );
            apply_attached_team_frame(
                &mut state,
                &mut requests,
                DaemonFrame::TeamViewDone {
                    request: key,
                    chunks: 1,
                },
                false,
                &tx,
            );
        }
        assert_eq!(state.thread_inspector.as_ref().unwrap().content, "latest");
        requests.pending.insert(
            "retract".into(),
            AttachedTeamRequest::Retract {
                next_sequence: 0,
                view: String::new(),
                view_done: false,
            },
        );
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamViewChunk {
                request: "retract".into(),
                sequence: 0,
                view: "remembered board".into(),
            },
            false,
            &tx,
        );
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamViewDone {
                request: "retract".into(),
                chunks: 1,
            },
            false,
            &tx,
        );
        assert_eq!(
            state.feedback_blocks["team-board"].message,
            "remembered board"
        );
        assert!(requests.pending.contains_key("retract"));
        apply_attached_team_frame(
            &mut state,
            &mut requests,
            DaemonFrame::TeamRetractAnswered {
                request: "retract".into(),
                message: "Landed".into(),
            },
            false,
            &tx,
        );
        assert_eq!(state.feedback_blocks["team-retract"].message, "Landed");
        assert!(requests.pending.is_empty());
    }
}

/// Story 21.11 (AC2/K2, attached host): the attached composer edits and paints
/// grapheme-safe through its real key/paste/draw seams.
#[cfg(test)]
mod display_width_tests {
    use super::*;
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};

    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const C: &str = "界";
    const A: &str = "e\u{301}";

    fn press(state: &mut TuiState, code: KeyCode, read_only: bool) {
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientFrame>();
        let mut requests = AttachedTeamRequests::default();
        attached_composer_key_with_team_requests(code, read_only, state, &tx, &mut requests);
        assert!(rx.try_recv().is_err(), "editing never emits a frame");
    }

    /// The production-shaped draw: `compute_layout` (Focus, no sidebar) then the
    /// shared `input_box::render` seam. Returns the painted rows, the
    /// caret and the composer rectangle.
    fn draw(state: &TuiState, width: u16, height: u16) -> (Vec<String>, Position, Rect) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut input_area = Rect::default();
        terminal
            .draw(|frame| {
                let display_input =
                    crate::adapters::tui::handlers::recipient_actions::pending_composition(state)
                        .unwrap_or(&state.input_buffer);
                let layout = crate::adapters::tui::layout::compute_layout(
                    frame.area(),
                    &state.theme,
                    display_input,
                    1,
                    false,
                    DensityMode::Focus,
                )
                .unwrap();
                input_area = layout.input_area;
                input_box::render(
                    frame,
                    layout.input_area,
                    display_input,
                    state.cursor_position,
                    state.focus.clone(),
                    &state.theme,
                    state.multiline_mode,
                    state.input_scroll_offset,
                    None,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0u16;
                while x < width {
                    let symbol = buffer[(x, y)].symbol().to_owned();
                    x += u16::try_from(crate::adapters::tui::text_geometry::cells(&symbol).max(1))
                        .unwrap();
                    row.push_str(&symbol);
                }
                row
            })
            .collect();
        (rows, terminal.get_cursor_position().unwrap(), input_area)
    }

    #[test]
    fn display_width_attached_backspace_removes_one_whole_grapheme() {
        let mut state = TuiState::new(80, 24);
        for (tail, expected) in [(G, "x"), (A, "x"), (C, "x"), ("y", "x")] {
            state.input_buffer = format!("x{tail}");
            state.cursor_position = state.input_buffer.chars().count();
            press(&mut state, KeyCode::Backspace, false);
            assert_eq!(state.input_buffer, expected, "tail {tail:?}");
        }
        // Read-only composers edit too; only submission is refused.
        state.input_buffer = format!("x{G}");
        state.cursor_position = state.input_buffer.chars().count();
        press(&mut state, KeyCode::Backspace, true);
        assert_eq!(state.input_buffer, "x");
        // Emptying the draft removes the whole final grapheme.
        state.input_buffer = A.to_owned();
        state.cursor_position = state.input_buffer.chars().count();
        press(&mut state, KeyCode::Backspace, false);
        assert!(state.input_buffer.is_empty());
    }

    #[test]
    fn display_width_attached_paste_then_backspace_keeps_source_bytes_of_the_rest() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = String::new();
        state.cursor_position = state.input_buffer.chars().count();
        attach_keys::handle_attached_paste(&mut state, &format!("a{A}{G}"), false);
        assert_eq!(
            state.input_buffer,
            format!("a{A}{G}"),
            "paste appends source bytes verbatim"
        );
        press(&mut state, KeyCode::Backspace, false);
        assert_eq!(state.input_buffer, format!("a{A}"));
        press(&mut state, KeyCode::Backspace, false);
        assert_eq!(state.input_buffer, "a");
    }

    #[test]
    fn display_width_attached_draw_wraps_and_shows_the_visual_tail_with_end_caret() {
        let mut state = TuiState::new(80, 24);
        // 60 columns: composer inner width 58; 30 C wrap to a second row.
        let input = C.repeat(30);
        state.input_buffer = input.clone();
        state.cursor_position = state.input_buffer.chars().count();
        let (rows, caret, area) = draw(&state, 60, 20);
        assert_eq!(area.height, 4, "two text rows + borders");
        let text_rows = &rows[usize::from(area.y) + 1..usize::from(area.y) + 3];
        assert_eq!(text_rows[0].matches('界').count(), 29, "{rows:?}");
        assert_eq!(text_rows[1].matches('界').count(), 1, "{rows:?}");
        assert_eq!(
            (caret.x, caret.y),
            (3, area.y + 2),
            "end caret after the last glyph"
        );

        // More than eight visual rows: the final window is shown, caret at its end.
        let long = format!("{}{G}", "x".repeat(58 * 9));
        state.input_buffer = long;
        state.cursor_position = state.input_buffer.chars().count();
        let (rows, caret, area) = draw(&state, 60, 24);
        assert_eq!(area.height, 10, "capped at eight text rows + borders");
        let text = rows[usize::from(area.y) + 1..usize::from(area.y) + 9].concat();
        assert!(
            text.contains(G),
            "the tail with the cluster is visible: {rows:?}"
        );
        assert_eq!(
            caret.y,
            area.y + 8,
            "end caret on the last visible text row"
        );
        assert!(caret.x > area.x && caret.x < area.x + area.width - 1);

        // Resize: the same draft re-wraps at the new width (no stale scroll).
        state.input_buffer = input;
        state.cursor_position = state.input_buffer.chars().count();
        let (_, _, narrow) = draw(&state, 60, 20);
        let (_, _, wide) = draw(&state, 130, 20);
        assert_eq!((narrow.height, wide.height), (4, 3));
    }

    #[test]
    fn display_width_attached_pending_composition_still_guards_editing() {
        let mut state = TuiState::new(80, 24);
        state.recipient_actions.insert(
            (
                crate::adapters::daemon::protocol::RecipientAction::Reply,
                crate::adapters::daemon::protocol::RecipientActionTarget::Item {
                    item_id: "ri".into(),
                },
            ),
            crate::adapters::tui::state::RecipientActionState::Pending {
                composed_input: Some(format!("pending {G}")),
                reply: None,
            },
        );
        state.input_buffer = String::new();
        state.cursor_position = state.input_buffer.chars().count();
        press(&mut state, KeyCode::Backspace, false);
        assert!(state.input_buffer.is_empty());
        assert_eq!(
            crate::adapters::tui::handlers::recipient_actions::pending_composition(&state),
            Some(format!("pending {G}").as_str()),
            "a pending composition owns the composer and is not edited"
        );
    }

    #[test]
    fn display_width_attached_consent_lines_wrap_by_cells_never_scalars() {
        let cells = crate::adapters::tui::text_geometry::cells;
        let wrap = |preview: &str| {
            crate::adapters::tui::widgets::modal::wrapped(
                preview,
                ratatui::style::Style::default(),
                40,
            )
        };
        let texts = |lines: Vec<ratatui::text::Line<'static>>| -> Vec<String> {
            lines
                .into_iter()
                .map(|line| {
                    line.spans
                        .into_iter()
                        .map(|s| s.content.into_owned())
                        .collect()
                })
                .collect()
        };
        // 30 C = 60 cells but only 30 scalars: the old scalar fast path kept
        // it as one overlong row at content width 40.
        let rows = texts(wrap(&C.repeat(30)));
        assert!(rows.len() >= 2, "{rows:?}");
        assert!(rows.iter().all(|row| cells(row) <= 40), "{rows:?}");
        assert_eq!(rows.concat().replace(' ', ""), C.repeat(30));
        // The shared word wrapper normalizes whitespace even when a line fits.
        let fits = format!("[y]  Allow {C}{C}  [n] Decline");
        assert_eq!(
            texts(wrap(&fits)),
            [format!("[y] Allow {C}{C} [n] Decline")]
        );
        // G and A at the cut stay whole.
        let mixed = format!("{}{G}{A}{}", "x".repeat(38), "y".repeat(20));
        let rows = texts(wrap(&mixed));
        assert!(rows.iter().all(|row| cells(row) <= 40), "{rows:?}");
        assert!(
            rows.concat().contains(G) && rows.concat().contains(A),
            "{rows:?}"
        );
    }

    #[test]
    fn display_width_attached_preview_and_consent_fit_cells() {
        let preview = preview_line(&C.repeat(72), 72);
        assert!(crate::adapters::tui::text_geometry::cells(&preview) <= 72);
        assert!(preview.ends_with('…'));
        assert_eq!(preview_line("short", 10), "short");
        assert_eq!(preview_line("a\nb", 10), "a b");
        let cut = preview_line(&format!("{G}xx"), 3);
        assert_eq!(cut, format!("{G}…"));
        assert_eq!(preview_line(&format!("{G}xx"), 2), "…");
        assert_eq!(preview_line(&format!("{G}xx"), 0), "");
        assert_eq!(preview_line(&format!("{A}{A}{A}"), 3), format!("{A}{A}{A}"));
    }
}
