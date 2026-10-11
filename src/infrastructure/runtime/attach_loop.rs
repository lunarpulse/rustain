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
use crate::adapters::tui::state::{AttachInfo, RecipientSurfaceTarget, TabRenderState, TuiState};
use crate::adapters::tui::terminal;
use crate::adapters::tui::widgets::recipient_surface::{
    RecipientSurfaceAction, pinned_response, recipient_surface_actions, render_recipient_surface,
};
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
            authorship: Default::default(),
            retracted_at_ms: None,
            history_source: Default::default(),
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
        history_source: turn.history_source,
        authorship: Default::default(),
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
        authorship: Default::default(),
        retracted_at_ms: None,
        history_source: Default::default(),
    }
}

// ── Story 19.19 (AC2) — host-authoritative peer row view ────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum PeerResponseStage<'a> {
    AwaitingOperator,
    Drafting,
    Ready(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPeerResponse<'a> {
    node: &'a str,
    stage: PeerResponseStage<'a>,
    queued: usize,
    node_resolvable: bool,
    canonical_type: &'static str,
    sender_label: &'a str,
    item_address: Option<&'a crate::domain::models::ItemAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingConsentApproval {
    request_id: crate::domain::models::RequestId,
    tool: String,
    input_preview: String,
}

fn pending_peer_response(conversation: &Conversation) -> Option<PendingPeerResponse<'_>> {
    use crate::domain::models::{PeerRowStage, semantic_message_type_metadata};
    let mut pendings = conversation.messages.iter().filter_map(|message| {
        let control = conversation.peer_row_controls.get(&message.id)?;
        if control.stage == PeerRowStage::Settled {
            return None;
        }
        let stage = match control.stage {
            PeerRowStage::AwaitingOperator => PeerResponseStage::AwaitingOperator,
            PeerRowStage::Drafting => PeerResponseStage::Drafting,
            PeerRowStage::Ready => PeerResponseStage::Ready(
                message
                    .content
                    .strip_prefix(crate::adapters::daemon::response_modes::DRAFT_APPROVAL_PREFIX)
                    .unwrap_or(&message.content),
            ),
            PeerRowStage::Settled => return None,
        };
        Some((control, stage))
    });
    let (control, stage) = pendings.next()?;
    Some(PendingPeerResponse {
        node: &control.node_id,
        stage,
        queued: pendings.count(),
        node_resolvable: control.node_resolvable,
        canonical_type: semantic_message_type_metadata(control.message_type).operator_noun,
        sender_label: &control.sender_label,
        item_address: control.item_address.as_ref(),
    })
}

/// The most recent unretracted agent-composed response supported by the daemon.
/// Diagnostic/tool-evidence rows have no response identity, and pending response
/// controls remain ineligible even when an edited draft is agent-composed.
fn latest_retractable(conversation: &Conversation) -> Option<ChatMessage> {
    conversation
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.authorship == crate::domain::models::MessageAuthorship::AgentComposed
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
        .cloned()
}

fn preview_line(content: &str, max_chars: usize) -> String {
    let mut chars = content.chars().map(|c| if c == '\n' { ' ' } else { c });
    let mut output = String::with_capacity(max_chars.saturating_mul(4));
    for _ in 0..max_chars {
        let Some(c) = chars.next() else { return output };
        output.push(c);
    }
    if max_chars > 0 && chars.next().is_some() {
        output.pop();
        output.push('…');
    }
    output
}

/// S1 retains status and preview; decisions require explicit non-Input focus.
fn peer_response_card_lines<'a>(
    pending: &PendingPeerResponse<'a>,
    read_only: bool,
) -> Vec<ratatui::text::Line<'a>> {
    use ratatui::text::{Line, Span};
    let queued = if pending.queued > 0 {
        format!("  [{} more queued]", pending.queued)
    } else {
        String::new()
    };
    let mut lines = Vec::with_capacity(3);
    lines.push(Line::from(vec![
        Span::raw(pending.canonical_type),
        Span::raw(" · "),
        Span::raw(pending.sender_label),
    ]));
    if pending.node_resolvable {
        match &pending.stage {
            PeerResponseStage::AwaitingOperator => {
                lines.push(Line::from(format!(
                    "Peer is waiting for your response.{queued}"
                )));
            }
            PeerResponseStage::Drafting => {
                lines.push(Line::from("Agent is drafting a reply to the peer…"));
            }
            PeerResponseStage::Ready(draft) => {
                lines.push(Line::from(format!(
                    "Peer draft: {}{queued}",
                    preview_line(draft, 72)
                )));
            }
        }
    }
    let passive = read_only
        || (pending.item_address.is_none()
            && (!pending.node_resolvable || matches!(pending.stage, PeerResponseStage::Drafting)));
    lines.push(Line::from(
        match (pending.item_address.is_some(), passive) {
            (true, false) => "[Tab] Item actions",
            (false, false) => "[Tab] Response actions",
            (true, true) => "[Tab] View item",
            (false, true) => "[Tab] View response",
        },
    ));
    lines
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

async fn open_response_surface(
    workspace: &Path,
    conversation: &Conversation,
    node: &str,
    state: &mut TuiState,
    read_only: bool,
    prefer_item: bool,
) -> Result<()> {
    use crate::adapters::tui::state::{
        RecipientSurface, RecipientSurfaceAccess, RecipientSurfaceTarget,
    };

    let (message, control) = pinned_response(conversation, node)
        .ok_or_else(|| anyhow!("response already settled — no action applied"))?;
    let access = if read_only {
        RecipientSurfaceAccess::ReadOnly
    } else {
        RecipientSurfaceAccess::Writer
    };
    if let Some(address) = &control.item_address {
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
        if prefer_item {
            return super::recipient_surface::open_received_surface(
                workspace,
                Some(conversation),
                address.item().as_str(),
                state,
                access,
                super::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await;
        }
    }
    let content = legacy_response_content(message, control);
    let (prior_focus, root_id) = match state.recipient_surface.take() {
        Some(surface) => (
            surface.prior_focus,
            if surface.node_id.as_deref() == Some(node) {
                surface.root_id
            } else {
                None
            },
        ),
        None => (std::mem::replace(&mut state.focus, FocusState::Chat), None),
    };
    state.focus = FocusState::Chat;
    state.recipient_surface = Some(RecipientSurface {
        target: RecipientSurfaceTarget::Node(node.to_owned()),
        access,
        can_toggle_target: control.item_address.is_some(),
        root_id,
        first_non_pinned_ordinal: None,
        last_non_pinned_ordinal: None,
        omitted_before: 0,
        omitted_after: 0,
        node_id: Some(node.to_owned()),
        item_state: None,
        context_available: false,
        context_selected: false,
        prior_focus,
        scroll_offset: 0,
        viewport_rows: 0,
        content,
        message_type: control.message_type,
        cached_width: 0,
        cached_lines: Vec::new(),
        cached_header: Vec::new(),
        cached_footer: Vec::new(),
        cached_action_mask: 0,
        results: Vec::new(),
    });
    state.needs_redraw = true;
    Ok(())
}

fn legacy_response_content(
    message: &ChatMessage,
    control: &crate::domain::models::PeerRowControl,
) -> String {
    use crate::domain::models::PeerRowStage;
    use crate::domain::services::transparency::sanitize_disclosable;
    let body = match (control.stage, control.node_resolvable) {
        (PeerRowStage::AwaitingOperator, true) => "Peer is waiting for your response.",
        (PeerRowStage::Drafting, true) => "Agent is drafting a reply to the peer…",
        (PeerRowStage::Ready, _) => message
            .content
            .strip_prefix(crate::adapters::daemon::response_modes::DRAFT_APPROVAL_PREFIX)
            .unwrap_or(&message.content),
        (PeerRowStage::Settled, _) => &message.content,
        (_, false) => "response already settled — no action applied",
    };
    let mut content = format!(
        "  {} · {}\n",
        crate::domain::models::semantic_message_type_metadata(control.message_type).operator_noun,
        sanitize_disclosable(&control.sender_label, control.sender_label.len()),
    );
    if super::recipient_view::safe_display_len(body)
        > crate::domain::services::recipient_thread_bounds::MAX_THREAD_TURN_BYTES
            .saturating_sub(content.len())
    {
        content.push_str("      turn content withheld · exceeds 448 KiB");
    } else {
        super::recipient_view::append_safe_body(&mut content, body);
    }
    content
}

fn recipient_surface_flash(state: &mut TuiState, message: String) {
    crate::adapters::tui::handlers::recipient_actions::show_notice(state, None, message);
}

fn queue_recipient_action(
    state: &mut TuiState,
    input: &mut String,
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
            | ClientFrame::ResolvePeerDraft { .. }
            | ClientFrame::SetRecipientContext { .. }
    );
    if correlated {
        if !begin_action(state, &frame, composed.then_some(&mut *input)) {
            return false;
        }
    } else if composed {
        input.clear();
    }
    if let Err(error) = frame_tx.send(frame) {
        let message = error.to_string();
        abort_unsent_action(state, input, error.0, message);
        return false;
    }
    true
}

/// Route an owned surface key, or leave it to a higher/lower-priority owner.
async fn dispatch_recipient_surface_key(
    key: crossterm::event::KeyEvent,
    higher_confirmation: bool,
    workspace: &Path,
    conversation: &Conversation,
    input: &mut String,
    state: &mut TuiState,
    write_frames: Option<&mpsc::UnboundedSender<ClientFrame>>,
) -> Option<bool> {
    use crossterm::event::{KeyCode, KeyModifiers};
    if state.recipient_surface.is_none()
        || higher_confirmation
        || (key.code, key.modifiers) == (KeyCode::Char('x'), KeyModifiers::CONTROL)
    {
        return None;
    }
    Some(recipient_surface_key(key, workspace, conversation, input, state, write_frames).await)
}
/// Returns true only for the existing attached detach chords.

async fn recipient_surface_key(
    key: crossterm::event::KeyEvent,
    _workspace: &Path,
    conversation: &Conversation,
    input: &mut String,
    state: &mut TuiState,
    write_frames: Option<&mpsc::UnboundedSender<ClientFrame>>,
) -> bool {
    use crate::adapters::daemon::protocol::PeerDraftAction;
    use crate::adapters::tui::state::{
        PeerComposeAction, PinnedPeerCompose, RecipientSurfaceTarget,
    };
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL)
    ) {
        return true;
    }
    if key.modifiers != KeyModifiers::NONE {
        return false;
    }
    match key.code {
        KeyCode::Esc => {
            if let Some(surface) = state.recipient_surface.take() {
                state.focus = surface.prior_focus;
            }
            crate::adapters::tui::handlers::recipient_actions::refresh_action_notices(state, false);
            state.needs_redraw = true;
            return false;
        }
        KeyCode::PageUp | KeyCode::PageDown => {
            if let Some(surface) = state.recipient_surface.as_mut() {
                surface.scroll_offset = if key.code == KeyCode::PageUp {
                    surface
                        .scroll_offset
                        .saturating_sub(surface.viewport_rows.saturating_sub(1).max(1))
                } else {
                    surface
                        .scroll_offset
                        .saturating_add(surface.viewport_rows.saturating_sub(1).max(1))
                };
            }
            state.needs_redraw = true;
            return false;
        }
        KeyCode::Tab => {
            let Some(surface) = state
                .recipient_surface
                .as_ref()
                .filter(|surface| surface.can_toggle_target)
            else {
                return false;
            };
            let target = match &surface.target {
                RecipientSurfaceTarget::Item(_) => surface.node_id.clone(),
                RecipientSurfaceTarget::Node(node) => Some(node.clone()),
            };
            if let Some(node) = target {
                let prefer_item = matches!(surface.target, RecipientSurfaceTarget::Node(_));
                if let Err(error) = open_response_surface(
                    _workspace,
                    conversation,
                    &node,
                    state,
                    write_frames.is_none(),
                    prefer_item,
                )
                .await
                {
                    recipient_surface_flash(state, error.to_string());
                }
            }
            return false;
        }
        KeyCode::Char('p' | 'l') => {
            if let Err(error) = super::recipient_surface::navigate_received_surface(
                _workspace,
                Some(conversation),
                state,
                key.code == KeyCode::Char('p'),
            )
            .await
            {
                recipient_surface_flash(state, error.to_string());
            }
            return false;
        }
        _ => {}
    }
    let read_only = write_frames.is_none();
    let Some(surface) = state.recipient_surface.as_ref() else {
        return false;
    };
    let Some(action) = recipient_surface_actions(state, conversation, read_only)
        .into_iter()
        .flatten()
        .find(|action| key.code == KeyCode::Char(action.key()))
    else {
        return false;
    };
    let Some(frame_tx) = write_frames else {
        return false;
    };
    match action {
        RecipientSurfaceAction::Respond
        | RecipientSurfaceAction::Discuss
        | RecipientSurfaceAction::Edit => {
            if !input.is_empty() {
                recipient_surface_flash(state,
                    "input already contains text — close this view and finish or clear it before composing a reply".to_owned());
                return false;
            }
            if action == RecipientSurfaceAction::Edit {
                let Some(node) = surface.node_id.as_deref() else {
                    return false;
                };
                let Some((message, _)) = pinned_response(conversation, node) else {
                    return false;
                };
                *input = message
                    .content
                    .strip_prefix(crate::adapters::daemon::response_modes::DRAFT_APPROVAL_PREFIX)
                    .unwrap_or(&message.content)
                    .to_owned();
                state.pinned_peer_compose = Some(PinnedPeerCompose::new(
                    node.to_owned(),
                    PeerComposeAction::Edit,
                ));
            } else {
                match &surface.target {
                    RecipientSurfaceTarget::Item(address) => {
                        *input = format!("/team reply {} ", address.item().as_str());
                        state.pinned_peer_compose = None;
                    }
                    RecipientSurfaceTarget::Node(node) => {
                        state.pinned_peer_compose = Some(PinnedPeerCompose::new(
                            node.clone(),
                            PeerComposeAction::Respond,
                        ));
                    }
                }
            }
            state.recipient_surface = None;
            state.focus = FocusState::Input;
            crate::adapters::tui::handlers::recipient_actions::refresh_action_notices(state, false);
        }
        RecipientSurfaceAction::Busy | RecipientSurfaceAction::Acknowledge => {
            let RecipientSurfaceTarget::Item(address) = &surface.target else {
                return false;
            };
            if action == RecipientSurfaceAction::Busy {
                // The card fast path reuses the durable principal/task join,
                // not its presentation sidecar as an item lookup authority.
                #[cfg(feature = "a2a")]
                if let Some(node) = surface.node_id.as_deref() {
                    let joined = match recipient_projection(_workspace).await {
                        Ok(projection) => crate::domain::models::AgentId::from_peer_path(node)
                            .ok()
                            .and_then(|node| {
                                crate::adapters::a2a::exec::recipient_item_for_node(
                                    &projection,
                                    &node,
                                )
                            }),
                        Err(error) => {
                            recipient_surface_flash(state, error.to_string());
                            return false;
                        }
                    };
                    if joined.is_none_or(|item| &item.address != address) {
                        recipient_surface_flash(
                            state,
                            format!("recipient item '{}' not found", address.item().as_str()),
                        );
                        return false;
                    }
                }
            }
            let item_id = address.item().as_str().to_owned();
            let frame = if action == RecipientSurfaceAction::Busy {
                ClientFrame::DeferRecipientItem { item_id }
            } else {
                ClientFrame::AcknowledgeRecipientItem { item_id }
            };
            queue_recipient_action(state, input, frame, frame_tx, false);
        }
        RecipientSurfaceAction::Context => {
            let RecipientSurfaceTarget::Item(address) = &surface.target else {
                return false;
            };
            let frame = ClientFrame::SetRecipientContext {
                item_id: address.item().as_str().to_owned(),
                selected: !surface.context_selected,
            };
            queue_recipient_action(state, input, frame, frame_tx, false);
        }
        RecipientSurfaceAction::Approve
        | RecipientSurfaceAction::Reject
        | RecipientSurfaceAction::Decline => {
            let Some(node) = surface.node_id.as_ref() else {
                return false;
            };
            let action = if action == RecipientSurfaceAction::Approve {
                PeerDraftAction::Approve
            } else {
                PeerDraftAction::Reject
            };
            let frame = ClientFrame::ResolvePeerDraft {
                node: node.clone(),
                action,
            };
            queue_recipient_action(state, input, frame, frame_tx, false);
        }
    }
    state.needs_redraw = true;
    false
}

/// The consent card's lines, each wrapped to the card's inner `width` when
/// longer than it (Story 19.18d `P22` dev smoke: the decision card renders
/// without wrap, so an overlong line — the bound `[a]` clause is 115
/// characters — was cut at the 60-column floor). A line that fits is kept
/// byte-for-byte, double spaces included.
fn consent_approval_card_lines(
    pending: &PendingConsentApproval,
    queued: usize,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    use crate::adapters::tui::widgets::chat_pane::word_wrap::wrap_text;

    let mut lines = Vec::new();
    for line in pending.input_preview.lines() {
        if width == 0 || line.chars().count() <= width {
            lines.push(ratatui::text::Line::from(line.to_owned()));
        } else {
            lines.extend(
                wrap_text(line, width)
                    .into_iter()
                    .map(ratatui::text::Line::from),
            );
        }
    }
    if queued > 0 {
        lines.push(ratatui::text::Line::from(format!(
            "  [{queued} more queued]"
        )));
    }
    lines
}

fn retract_confirm_lines(preview: &str) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line;
    vec![
        Line::from(format!("Retract this auto-sent message? {}", preview)),
        Line::from("[y] Retract — marked in your log, never deleted  [n] Keep".to_owned()),
    ]
}

fn tool_call_info_from_transition(call: &ToolCall) -> ToolCallInfo {
    let request = call.request();
    let result = match call {
        ToolCall::Success { result, .. } => Some(ToolResultInfo {
            content: result.output.clone(),
            is_error: result.is_error,
            diff: crate::domain::models::WriteDiffState::NotAWrite,
        }),
        ToolCall::Error { error, .. } => Some(ToolResultInfo {
            content: error.clone(),
            is_error: true,
            diff: crate::domain::models::WriteDiffState::NotAWrite,
        }),
        ToolCall::Cancelled { reason, .. } => Some(ToolResultInfo {
            content: reason.clone(),
            is_error: true,
            diff: crate::domain::models::WriteDiffState::NotAWrite,
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
    if snapshot.blocked_actions_waiting > 0 {
        conv.messages.push(system_message(format!(
            "⚠ {} action(s) waiting on you — denied while no client was attached. Re-run or approve now.",
            snapshot.blocked_actions_waiting
        )));
    }
    conv
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
/// Extracted so the intercept table is reachable by a test: the match it holds
/// is **not** compile-forced. A new `/team` verb with no arm here compiles,
/// falls through to `driver.submit`, and becomes a model prompt — which is
/// exactly what happened to `log`, `trust`, `status` and `send`.
#[derive(Debug)]
enum AttachedTeamLine {
    /// An intercepted verb: send this frame; never a model turn.
    Frame(ClientFrame),
    /// A malformed `/team` line: flash the parser's usage error; never a model
    /// turn.
    Refused(String),
    /// Not intercepted here — the pre-existing fall-through to the model.
    PassThrough,
    /// Story 19.16g — `/team log …`: a local read of this workspace's journal,
    /// rendered in-chat; ⛔ never a model turn and never a daemon frame.
    LocalLog(crate::adapters::tui::handlers::team_command::TeamLogArgs),
    LocalView(String),
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
/// exception is reachable by a test. A read-only client may now edit its
/// composer and run the **local** `/team log`; every other submission — a
/// model turn, a draft resolution, a daemon-mutating `/team` frame — stays
/// behind the existing read-only refusal, with no frame sent.
fn attached_composer_key(
    code: crossterm::event::KeyCode,
    read_only: bool,
    input: &mut String,
    state: &mut TuiState,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
) -> ComposerOutcome {
    use crossterm::event::KeyCode;
    let flash = |state: &mut TuiState, message: String| {
        state.status = StatusState::Flash {
            message,
            remaining_ms: 1500,
        };
        state.needs_redraw = true;
    };
    if crate::adapters::tui::handlers::recipient_actions::pending_composition(state).is_some() {
        return ComposerOutcome::Handled;
    }
    match code {
        KeyCode::Char(c) => input.push(c),
        KeyCode::Backspace => {
            input.pop();
            if input.is_empty() {
                state.pinned_peer_compose = None;
            }
        }
        KeyCode::Enter if input.trim().is_empty() => {
            if read_only {
                // AC6 #3 — inert, never silent.
                flash(state, "read-only — can't send here".into());
            }
        }
        KeyCode::Enter => match attached_team_line(input) {
            AttachedTeamLine::LocalLog(args) => {
                input.clear();
                state.pinned_peer_compose = None;
                return ComposerOutcome::LocalLog(args);
            }
            AttachedTeamLine::LocalView(item_id) => {
                input.clear();
                state.pinned_peer_compose = None;
                return ComposerOutcome::LocalView(item_id);
            }
            AttachedTeamLine::Frame(ClientFrame::InspectRecipientContext) => {
                state.pinned_peer_compose = None;
                if queue_recipient_action(
                    state,
                    input,
                    ClientFrame::InspectRecipientContext,
                    frame_tx,
                    true,
                ) {
                    return ComposerOutcome::FrameSent;
                }
            }
            // AC6 #3 — inert, never silent; the line stays editable.
            _ if read_only => flash(state, "read-only — can't send here".into()),
            AttachedTeamLine::Frame(frame) => {
                state.pinned_peer_compose = None;
                if !queue_recipient_action(state, input, frame, frame_tx, true) {
                    return ComposerOutcome::Handled;
                }
                return ComposerOutcome::FrameSent;
            }
            // A malformed `/team` line must never become a model turn —
            // consume it and show the parser's usage error.
            AttachedTeamLine::Refused(error) => {
                input.clear();
                state.pinned_peer_compose = None;
                flash(state, error);
            }
            AttachedTeamLine::PassThrough => {
                if is_attached_command(input) {
                    state.pinned_peer_compose = None;
                }
                return ComposerOutcome::Submit(std::mem::take(input));
            }
        },
        _ => {}
    }
    ComposerOutcome::Handled
}

fn is_attached_command(text: &str) -> bool {
    let text = text.trim();
    text == "/clear"
        || text
            .strip_prefix("/team")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

fn pinned_compose_is_current(
    conversation: &Conversation,
    compose: &crate::adapters::tui::state::PinnedPeerCompose,
) -> bool {
    use crate::adapters::tui::state::PeerComposeAction;
    use crate::domain::models::PeerRowStage;
    pinned_response(conversation, &compose.node_id).is_some_and(|(_, control)| {
        control.node_resolvable
            && match compose.action {
                PeerComposeAction::Respond => matches!(
                    control.stage,
                    PeerRowStage::AwaitingOperator | PeerRowStage::Ready
                ),
                PeerComposeAction::Edit => control.stage == PeerRowStage::Ready,
            }
    })
}

fn take_current_peer_compose(
    state: &mut TuiState,
    conversation: &Conversation,
) -> Result<Option<crate::adapters::tui::state::PinnedPeerCompose>, ()> {
    let Some(compose) = state.pinned_peer_compose.as_ref() else {
        return Ok(None);
    };
    if !pinned_compose_is_current(conversation, compose) {
        return Err(());
    }
    Ok(state.pinned_peer_compose.take())
}

/// Story 19.16g — after the attached draw: only a frame that actually
/// completed presents the `team-log` block it painted into the chat viewport.
/// A failed draw, or a layout too small to paint the chat, presents nothing.
fn after_attached_draw(state: &mut TuiState, drawn: bool, visible_feedback_ids: &[String]) {
    if drawn {
        state.log_visits_presented(visible_feedback_ids);
    }
}

fn attached_team_line(text: &str) -> AttachedTeamLine {
    use crate::adapters::tui::handlers::team_command::{TeamCommandArgs, parse_team_command};

    if text.trim() == "/clear" {
        return AttachedTeamLine::Frame(ClientFrame::UserMessage {
            text: "/clear".to_owned(),
            images: Vec::new(),
        });
    }
    // Story 19.17: accept the `/team` word only at an input boundary.
    // Bare `/team` and whitespace-delimited verbs are local; `/teamwork`
    // remains an ordinary model prompt. Leading pasted whitespace is ignored
    // so an attached send or removal cannot evade the intercept table.
    let line = text.trim_start();
    let Some(arg) = line.strip_prefix("/team") else {
        return AttachedTeamLine::PassThrough;
    };
    if !arg.is_empty() && !arg.starts_with(char::is_whitespace) {
        return AttachedTeamLine::PassThrough;
    }
    let arg = arg.trim_start();
    match parse_team_command(Some(arg)) {
        Ok(TeamCommandArgs::Acknowledge { item_id }) => {
            AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { item_id })
        }
        // A destructive verb must not become a model prompt either: the
        // daemon's own refusal reaches the operator as `[daemon error] …`.
        Ok(TeamCommandArgs::Remove { item_id }) => {
            AttachedTeamLine::Frame(ClientFrame::RemoveRecipientItem { item_id })
        }
        // Story 19.16b AC3(h)1 — ⛔ NOT a fall-through. The board reads every
        // configured peer over this session's OWN A2A egress, and an attached
        // session holds none: `AttachServer` is composed without an
        // `A2aDelegationRuntime` (the daemon installs its egress on the
        // capability composite, never on the attach server). Falling through
        // would turn `/team board` into an LLM prompt — exactly what happened
        // to `log`, `trust`, `status` and `send`. Refuse aloud instead, and
        // say where the verb does work.
        // ⚠ Sized for the 1.5 s single-line status flash `Refused` routes to
        // (19.16b review): the actionable clause must survive a narrow
        // terminal, so the explanation lives in the comment, not the flash.
        Ok(TeamCommandArgs::Board { .. }) => AttachedTeamLine::Refused(
            "'/team board' needs this session's own A2A egress — run it in a non-attached \
             session."
                .to_owned(),
        ),
        // Story 19.16f AC3(c) — the cross-host retract, the SECOND verb that
        // refuses aloud here (`DF-19-16B-BOARD-ABSENT-ON-THE-ATTACHED-RAIL`,
        // owner 19.21): it is a write over this session's own egress, which an
        // attached session does not hold. ⛔ Falling through would type a
        // destructive verb at the model. ⛔ Not the same-host `Ctrl+X` retract
        // either — that retracts this host's auto-sent message, a different
        // object, and its frame is untouched.
        Ok(TeamCommandArgs::Retract { .. }) => AttachedTeamLine::Refused(
            "'/team retract' needs this session's own A2A egress — run it in a non-attached \
             session."
                .to_owned(),
        ),
        // Story 19.17: the attached client has no per-session A2A send egress.
        Ok(TeamCommandArgs::Send { .. }) => AttachedTeamLine::Refused(
            "'/team send' needs this session's own A2A egress — run it in a non-attached \
             session."
                .to_owned(),
        ),
        // Story 19.18 `AC9(a)` — the recipient answers an item that arrived
        // on THIS host: the one new verb rail 4 serves, as a daemon frame.
        Ok(TeamCommandArgs::Reply { item_id, text }) => {
            AttachedTeamLine::Frame(ClientFrame::ReplyToRecipientItem { item_id, text })
        }
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
        // Story 19.18 `AC9(a)` — the asker's verbs read and write a PEER's
        // host over this session's own egress, which an attached session does
        // not hold. ⛔ Never a fall-through to the model (`M24`).
        Ok(TeamCommandArgs::Thread { .. }) => AttachedTeamLine::Refused(
            "'/team thread' needs this session's own A2A egress — run it in a non-attached \
             session."
                .to_owned(),
        ),
        Ok(TeamCommandArgs::Followup { .. }) => AttachedTeamLine::Refused(
            "'/team followup' needs this session's own A2A egress — run it in a non-attached \
             session."
                .to_owned(),
        ),
        // Story 19.16g — a local read of this workspace's journal; the
        // attached client is a Unix-socket client of the same filesystem.
        Ok(TeamCommandArgs::Log(args)) => AttachedTeamLine::LocalLog(args),
        // Other `/team` verbs keep their pre-existing fall-through.
        Ok(_) => AttachedTeamLine::PassThrough,
        Err(error) => AttachedTeamLine::Refused(error),
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
    let mut retract_confirm: Option<(String, String)> = None;
    // Consent cards are daemon-owned and re-emitted after attach. This FIFO is
    // only the current socket's rendering order, never the durable truth.
    let mut pending_consent_approvals = std::collections::VecDeque::<PendingConsentApproval>::new();

    // ── Terminal ──
    let mut term = terminal::setup(false)?;
    let size = term.size()?;
    let mut state = TuiState::new(size.width, size.height);
    state.recipient_context_ids = snapshot.recipient_context_ids.iter().cloned().collect();
    if read_only {
        state.focus = FocusState::Chat;
    }
    let mut tab_render_state = TabRenderState::default();
    let tool_block_states: HashMap<
        String,
        crate::adapters::tui::widgets::tool_block::ToolBlockState,
    > = HashMap::new();
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
    // Attached clients render the same feedback blocks as the local TUI so
    // forwarded transparency room/failure events have the same visible result.

    use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
    use futures::StreamExt;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let loop_result: Result<()> = loop {
        let mut visible_feedback_ids: Vec<String> = Vec::new();
        let drawn = term.draw(|f| {
            let area = f.area();
            let Some(layout) = crate::adapters::tui::layout::compute_layout(
                area,
                &state.theme,
                crate::adapters::tui::handlers::recipient_actions::pending_composition(&state).unwrap_or(&input),
                1,
                false,
                DensityMode::Focus,
            ) else {
                return;
            };
            let peer_card = if state.recipient_surface.is_none()
                && retract_confirm.is_none()
                && pending_consolidation_card.is_none()
                && pending_consent_approvals.is_empty()
            {
                pending_peer_response(&conversation)
                    .map(|pending| peer_response_card_lines(&pending, read_only))
            } else {
                None
            };
            let mut transcript_area = layout.chat_pane;
            if let Some(lines) = peer_card.as_ref() {
                transcript_area.height = transcript_area.height.saturating_sub(
                    u16::try_from(lines.len()).unwrap_or(u16::MAX).saturating_add(2),
                );
            }
            visible_feedback_ids = chat_pane::render_attached(
                f,
                transcript_area,
                &conversation,
                &streaming,
                scroll_offset,
                auto_scroll,
                &state.theme,
                &mut tab_render_state,
                &tool_block_states,
                &state.feedback_blocks,
                state.active_feedback_id.as_deref(),
            )
            .visible_feedback_ids;
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
                state.recipient_surface.is_none().then_some("Esc/Ctrl+D/Ctrl+C detach"),
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
            state.log_awareness.display,
);
            let display_input = crate::adapters::tui::handlers::recipient_actions::pending_composition(&state).unwrap_or(&input);
            input_box::render(
                f,
                layout.input_area,
                display_input,
                display_input.chars().count(),
                state.focus.clone(),
                &state.theme,
                false,
                0,
                if read_only {
                    Some("read-only — can't send here")
                } else {
                    state.pinned_peer_compose.as_ref().map(|compose| compose.label.as_str())
                },
            );
            let actions = recipient_surface_actions(&state, &conversation, read_only);
            if let Some(surface) = state.recipient_surface.as_mut() {
                render_recipient_surface(f, layout.chat_pane, surface, &state.recipient_actions,
                    &actions, read_only, &state.theme);
            }
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
            // Story 18.3c AC3/AC5 — the peer response card (and the retract
            // confirmation, which takes precedence while open) ride the same
            // bottom-anchored inline grammar as the consolidation card.
            use crate::adapters::tui::widgets::inline_card::{
                render_bottom_anchored_card, render_bottom_anchored_decision_card,
            };
            if let Some((_, ref preview)) = retract_confirm {
                render_bottom_anchored_card(
                    f.buffer_mut(),
                    retract_confirm_lines(preview),
                    state.theme.colors.auto_sent_border,
                    layout.chat_pane,
                );
            } else if let Some(lines) = peer_card {
                render_bottom_anchored_card(
                    f.buffer_mut(),
                    lines,
                    state.theme.colors.accent,
                    layout.chat_pane,
                );
            }
            if let Some(pending) = pending_consent_approvals.front() {
                render_bottom_anchored_decision_card(
                    f.buffer_mut(),
                    consent_approval_card_lines(
                        pending,
                        pending_consent_approvals.len().saturating_sub(1),
                        usize::from(layout.chat_pane.width.saturating_sub(2)),
                    ),
                    state.theme.colors.accent,
                    layout.chat_pane,
                );
            }
        });
        after_attached_draw(&mut state, drawn.is_ok(), &visible_feedback_ids);

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        let higher_confirmation = !pending_consent_approvals.is_empty()
                            || pending_consolidation_card.is_some() || retract_confirm.is_some();
                        if key.kind != KeyEventKind::Press
                            && (state.recipient_surface.is_some() || higher_confirmation)
                        {
                            continue;
                        }
                        if let Some(detach) = dispatch_recipient_surface_key(
                            key, higher_confirmation, workspace, &conversation,
                            &mut input, &mut state, (!read_only).then_some(&frame_tx),
                        ).await {
                            if detach {
                                break Ok(());
                            }
                            continue;
                        }
                        if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE
                            && !higher_confirmation && key.kind == KeyEventKind::Press
                            && let Some(pending) = pending_peer_response(&conversation)
                        {
                            let node = pending.node.to_owned();
                            if let Err(error) = open_response_surface(
                                workspace, &conversation, &node, &mut state, read_only, true,
                            ).await {
                                recipient_surface_flash(&mut state, error.to_string());
                            }
                            continue;
                        }
                        // Any further keypress dismisses the retract confirmation.
                        state.feedback_blocks.remove("peer-retract");
                        match (key.code, key.modifiers) {
                            // Story 18.3d AC1 — sender-consent Approval Pattern.
                            // The guard is the daemon-derived pending FIFO, so
                            // these single-letter keys are inert everywhere else.
                            (KeyCode::Char('y'), m) if !m.contains(KeyModifiers::CONTROL)
                                && !pending_consent_approvals.is_empty() =>
                            {
                                if let Some(pending) = pending_consent_approvals.pop_front() {
                                    let _ = frame_tx.send(ClientFrame::ApprovalResponse {
                                        request_id: pending.request_id,
                                        outcome: crate::domain::models::ApprovalOutcome::Once,
                                    });
                                }
                                state.needs_redraw = true;
                            }
                            (KeyCode::Char('a'), m) if !m.contains(KeyModifiers::CONTROL)
                                && !pending_consent_approvals.is_empty() =>
                            {
                                if let Some(pending) = pending_consent_approvals.pop_front() {
                                    let _ = frame_tx.send(ClientFrame::ApprovalResponse {
                                        request_id: pending.request_id,
                                        outcome: crate::domain::models::ApprovalOutcome::AlwaysAndSave {
                                            scope: crate::domain::models::ApprovalScope::Tool(
                                                pending.tool,
                                            ),
                                        },
                                    });
                                }
                                state.needs_redraw = true;
                            }
                            (KeyCode::Char('n'), m) | (KeyCode::Esc, m)
                                if !m.contains(KeyModifiers::CONTROL)
                                    && !pending_consent_approvals.is_empty() =>
                            {
                                if let Some(pending) = pending_consent_approvals.pop_front() {
                                    let _ = frame_tx.send(ClientFrame::ApprovalResponse {
                                        request_id: pending.request_id,
                                        outcome: crate::domain::models::ApprovalOutcome::Reject {
                                            feedback: None,
                                        },
                                    });
                                }
                                state.needs_redraw = true;
                            }
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
                            // Story 18.3c AC5 — the retract confirmation owns
                            // y/n/Esc while open (Approval grammar, never a
                            // bare key on a transient surface).
                            (KeyCode::Char('y'), m) if !m.contains(KeyModifiers::CONTROL)
                                && retract_confirm.is_some() =>
                            {
                                if read_only {
                                    state.status = StatusState::Flash {
                                        message: "read-only — can't retract here".into(),
                                        remaining_ms: 1500,
                                    };
                                } else if let Some((message_id, _)) = retract_confirm.take() {
                                    let _ = frame_tx.send(ClientFrame::RetractAutoResponse {
                                        message_id,
                                        target_seq: None,
                                    });
                                    state.feedback_blocks.insert(
                                        "peer-retract".to_owned(),
                                        crate::domain::models::FeedbackBlock {
                                            id: "peer-retract".to_owned(),
                                            level: crate::domain::models::FeedbackLevel::Info,
                                            message: "Retracted. Marked in your log — never deleted.\nWhat was already read, was read."
                                                .to_owned(),
                                            actions: vec![],
                                        },
                                    );
                                }
                                state.needs_redraw = true;
                            }
                            (KeyCode::Char('n'), m) | (KeyCode::Esc, m)
                                if !m.contains(KeyModifiers::CONTROL)
                                    && retract_confirm.is_some() =>
                            {
                                retract_confirm = None;
                                state.needs_redraw = true;
                            }
                            // Story 18.3c AC5 — Ctrl+X opens the retract
                            // confirmation for the most recent auto-sent row.
                            (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
                                if read_only {
                                    state.status = StatusState::Flash {
                                        message: "read-only — can't retract here".into(),
                                        remaining_ms: 1500,
                                    };
                                } else if let Some(row) = latest_retractable(&conversation) {
                                    retract_confirm =
                                        Some((row.id.clone(), preview_line(&row.content, 48)));
                                } else {
                                    state.status = StatusState::Flash {
                                        message: "no auto-sent message to retract".into(),
                                        remaining_ms: 1500,
                                    };
                                }
                                state.needs_redraw = true;
                            }
                            // AC4 — detach (keybinding only, no CLI verb).
                            (KeyCode::Esc, _)
                            | (KeyCode::Char('d'), KeyModifiers::CONTROL)
                            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                break Ok(());
                            }
                            (KeyCode::Enter, _) | (KeyCode::Backspace, _) => {
                                if state.recipient_surface.is_some() {
                                    continue;
                                }
                                let text = match attached_composer_key(
                                    key.code,
                                    read_only,
                                    &mut input,
                                    &mut state,
                                    &frame_tx,
                                ) {
                                    ComposerOutcome::Handled => continue,
                                    ComposerOutcome::FrameSent => {
                                        auto_scroll = true;
                                        continue;
                                    }
                                    // Story 19.16g — a user-initiated log may
                                    // scroll its own result into view.
                                    ComposerOutcome::LocalLog(args) => {
                                        for notice in crate::infrastructure::runtime::transparency_bridge::attached_team_log(
                                            &log_service,
                                            &mut state,
                                            &args,
                                        )
                                        .await
                                        {
                                            conversation.messages.push(system_message(notice));
                                        }
                                        auto_scroll = true;
                                        continue;
                                    }
                                    ComposerOutcome::LocalView(item_id) => {
                                        let access = if read_only {
                                            crate::adapters::tui::state::RecipientSurfaceAccess::ReadOnly
                                        } else {
                                            crate::adapters::tui::state::RecipientSurfaceAccess::Writer
                                        };
                                        if let Err(error) = super::recipient_surface::open_received_surface(
                                            workspace, Some(&conversation), &item_id, &mut state, access,
                                            super::recipient_view::ReceivedThreadWindow::Newest,
                                        ).await {
                                            recipient_surface_flash(&mut state, error.to_string());
                                        }
                                        continue;
                                    }
                                    ComposerOutcome::Submit(text) => text,
                                };
                                let compose = match take_current_peer_compose(&mut state, &conversation) {
                                    Ok(compose) => compose,
                                    Err(()) => {
                                        input = text;
                                        recipient_surface_flash(&mut state,
                                            "response already settled — your text was not sent".to_owned());
                                        continue;
                                    }
                                };
                                if let Some(compose) = compose {
                                    input.clone_from(&text);
                                    let action = match compose.action {
                                        crate::adapters::tui::state::PeerComposeAction::Respond =>
                                            crate::adapters::daemon::protocol::PeerDraftAction::WriteOwn { content: text },
                                        crate::adapters::tui::state::PeerComposeAction::Edit =>
                                            crate::adapters::daemon::protocol::PeerDraftAction::Edit { content: text },
                                    };
                                    let node = compose.node_id.clone();
                                    state.pinned_peer_compose = Some(compose);
                                    let frame = ClientFrame::ResolvePeerDraft { node, action };
                                    queue_recipient_action(&mut state, &mut input, frame, &frame_tx, true);
                                } else {
                                    let sub = UserSubmission {
                                        text,
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
                            (KeyCode::Char(_), m) if !m.contains(KeyModifiers::CONTROL) => {
                                if state.recipient_surface.is_some() {
                                    continue;
                                }
                                // Story 19.16g: read-only composers edit too;
                                // only submission is refused.
                                attached_composer_key(
                                    key.code,
                                    read_only,
                                    &mut input,
                                    &mut state,
                                    &frame_tx,
                                );
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        if state.recipient_surface.is_none()
                            && crate::adapters::tui::handlers::recipient_actions::pending_composition(&state).is_none()
                            && pending_consent_approvals.is_empty()
                            && pending_consolidation_card.is_none()
                            && retract_confirm.is_none()
                        {
                            input.push_str(&text);
                            state.needs_redraw = true;
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
                        Ok(Some(DaemonFrame::PeerRowOperation(operation))) => {
                            let refresh_node = state.recipient_surface.as_ref().is_some_and(|surface| {
                                let RecipientSurfaceTarget::Node(node) = &surface.target else { return false };
                                let (row_id, new_control) = match operation.as_ref() {
                                    crate::domain::models::PeerRowOperation::Upsert { message, control, .. } =>
                                        (message.id.as_str(), control.as_ref()),
                                    crate::domain::models::PeerRowOperation::Remove { row_id, .. } =>
                                        (row_id.as_str(), None),
                                };
                                new_control.or_else(|| conversation.peer_row_controls.get(row_id))
                                    .is_some_and(|control| &control.node_id == node)
                            });
                            let refresh_item = state.recipient_surface.as_ref().is_some_and(|surface| {
                                let RecipientSurfaceTarget::Item(address) = &surface.target else { return false };
                                let (row_id, new_control) = match operation.as_ref() {
                                    crate::domain::models::PeerRowOperation::Upsert { message, control, .. } =>
                                        (message.id.as_str(), control.as_ref()),
                                    crate::domain::models::PeerRowOperation::Remove { row_id, .. } =>
                                        (row_id.as_str(), None),
                                };
                                new_control
                                    .or_else(|| conversation.peer_row_controls.get(row_id))
                                    .is_some_and(|control| control.item_address.as_ref() == Some(address))
                            });
                            if apply_peer_row_operation(&mut conversation, Arc::unwrap_or_clone(operation)) {
                                if refresh_node && let Some(surface) = state.recipient_surface.as_mut()
                                    && let RecipientSurfaceTarget::Node(node) = &surface.target
                                {
                                    if let Some((message, control)) = pinned_response(&conversation, node) {
                                        surface.content = legacy_response_content(message, control);
                                        surface.can_toggle_target = control.item_address.is_some();
                                    } else {
                                        surface.content = "response already settled — no action applied".to_owned();
                                        surface.can_toggle_target = false;
                                    }
                                    surface.cached_width = 0;
                                }
                                if refresh_item {
                                    super::recipient_surface::reconcile_item_surface(&mut state, &conversation);
                                }
                                state.needs_redraw = true;
                            }
                        }
                        Ok(Some(DaemonFrame::Event(raw))) => {
                            apply_client_event(
                                raw,
                                &mut conversation,
                                &mut streaming,
                                &mut reducer,
                                &mut state,
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
                    Ok(Some(DaemonFrame::ApprovalRequest {
                        tool,
                        request_id,
                        input_preview,
                        ..
                    })) if tool == "a2a/sender-consent" => {
                        pending_consent_approvals.push_back(PendingConsentApproval {
                            request_id,
                            tool,
                            input_preview,
                        });
                        state.needs_redraw = true;
                    }
                    Ok(Some(DaemonFrame::ApprovalRequest { tool, request_id, .. })) => {
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
                    Ok(Some(DaemonFrame::RecipientActionResult { action, target, outcome })) => {
                        crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                            &mut state, &mut input, action, target, outcome,
                        );
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
            _ = tick.tick() => {
                log_awareness.tick(&mut state.log_awareness);
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
mod tests {
    use super::*;
    use crate::domain::models::{NoticeLevel, StopReason, StreamChunk};
    use std::time::Instant;
    use tokio::net::UnixStream;

    #[test]
    fn consent_card_queue_indicator_is_derived_from_the_daemon_fifo() {
        let pending = PendingConsentApproval {
            request_id: crate::domain::models::RequestId::new(),
            tool: "a2a/sender-consent".to_owned(),
            input_preview: "Consent required\n[y] Allow once  [a] Always allow  [n] Decline"
                .to_owned(),
        };
        let text = consent_approval_card_lines(&pending, 2, 200)
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[2 more queued]"));
        assert!(text.contains("[y] Allow once  [a] Always allow  [n] Decline"));
    }

    /// Story 19.18d `P22` dev smoke, kept as a regression: at the 60-column
    /// layout floor the bound card's `[a]` line renders whole — wrapped inside
    /// the decision card, never cut — and a line that fits keeps its bytes.
    #[test]
    fn an_overlong_consent_line_wraps_inside_the_card_at_the_layout_floor() {
        use crate::adapters::tui::widgets::inline_card::render_bottom_anchored_decision_card;
        use crate::domain::services::peer_admission::BOUND_KEY_CONSENT_LINE;

        let pin = crate::domain::models::PeerId::from_public_key(&[9; 32]).unwrap();
        let pending = PendingConsentApproval {
            request_id: crate::domain::models::RequestId::new(),
            tool: "a2a/sender-consent".to_owned(),
            input_preview: crate::infrastructure::runtime::transparency_bridge::consent_card_text(
                &crate::domain::models::PolicyPrincipal::bound_a2a(pin),
            ),
        };
        let pane = ratatui::layout::Rect::new(0, 0, 60, 30);
        let lines = consent_approval_card_lines(&pending, 0, usize::from(pane.width - 2));
        let mut buffer = ratatui::buffer::Buffer::empty(pane);
        render_bottom_anchored_decision_card(
            &mut buffer,
            lines.clone(),
            ratatui::style::Color::Reset,
            pane,
        );
        let rendered: String = (0..pane.height)
            .map(|y| {
                (1..pane.width - 1)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ");
        let words = |text: &str| {
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let rendered_words = words(&rendered);
        let clause = words(&format!("[a] {BOUND_KEY_CONSENT_LINE}"));
        assert!(
            rendered_words
                .windows(clause.len())
                .any(|window| window == clause.as_slice()),
            "the whole bound clause is visible at 60 columns: {rendered}"
        );
        let kept = "[y] Allow once  [a] Always allow  [n] Decline";
        assert!(
            lines.iter().any(|line| line.to_string() == kept),
            "a line that fits keeps its bytes"
        );
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
            message_type: crate::domain::models::SemanticMessageType::Consultation,
            sender_label: "sender".to_owned(),
            stage,
            node_resolvable: true,
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

        let address = ItemAddress::from_rap_ingress(
            PeerId::from_public_key(&[73; 32]).unwrap(),
            ItemId::from_replay(item_id),
        );
        journal
            .append_room(RoomEvent::RecipientItemReceived {
                address: address.clone(),
                task: format!("task-{item_id}"),
                alias: Some("test sender".to_owned()),
                message_type,
                content: content.to_owned(),
                in_reply_to: None,
                capability_hash: None,
                reply_capability_matched: false,
            })
            .await
            .unwrap();
        address
    }

    #[tokio::test]
    async fn explicit_response_focus_freezes_input_and_filters_action_events() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let workspace = tempfile::tempdir().unwrap();
        let mut conversation = Conversation {
            messages: vec![peer_row("row-a", "incoming")],
            peer_row_controls: [(
                "row-a".to_owned(),
                peer_control(
                    "node-a",
                    crate::domain::models::PeerRowStage::AwaitingOperator,
                ),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Input;
        let mut input = "unfinished operator text".to_owned();
        let (tx, mut rx) = mpsc::unbounded_channel();
        open_response_surface(
            workspace.path(),
            &conversation,
            "node-a",
            &mut state,
            false,
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            state.focus,
            FocusState::Chat,
            "opening the scoped response surface takes non-Input focus"
        );
        for key in [
            KeyEvent::new_with_kind(KeyCode::Char('n'), KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new_with_kind(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        ] {
            assert!(
                !recipient_surface_key(
                    key,
                    workspace.path(),
                    &conversation,
                    &mut input,
                    &mut state,
                    Some(&tx)
                )
                .await
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "non-Press and ineligible keys cannot dispatch"
        );
        assert_eq!(input, "unfinished operator text");
        assert!(
            state.recipient_surface.is_some(),
            "nonempty compose refusal retains focus"
        );
        conversation
            .messages
            .insert(0, peer_row("row-b", "later arrival"));
        conversation.peer_row_controls.insert(
            "row-b".to_owned(),
            peer_control("node-b", crate::domain::models::PeerRowStage::Ready),
        );
        recipient_surface_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            workspace.path(),
            &conversation,
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        assert!(
            matches!(rx.try_recv().unwrap(), ClientFrame::ResolvePeerDraft {
            node, action: crate::adapters::daemon::protocol::PeerDraftAction::Reject
        } if node == "node-a"),
            "a new FIFO head cannot retarget explicit focus"
        );
        recipient_surface_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            workspace.path(),
            &conversation,
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        assert!(state.recipient_surface.is_none());
        assert_eq!(state.focus, FocusState::Input);
        assert_eq!(input, "unfinished operator text");
    }

    #[tokio::test]
    async fn answerable_other_and_unknown_items_respond_without_discuss() {
        use crate::adapters::tui::state::{RecipientSurfaceAccess, RecipientSurfaceTarget};
        use crate::adapters::tui::widgets::recipient_surface::RecipientSurfaceAction;
        use crate::domain::models::{PeerRowStage, SemanticMessageType};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
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
                    message_type,
                    sender_label: "test sender".to_owned(),
                    stage: PeerRowStage::AwaitingOperator,
                    node_resolvable: true,
                },
            );
            addresses.push(address);
        }

        let mut state = TuiState::new(80, 24);
        let mut input = String::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        for address in addresses {
            input.push_str("unfinished operator text");
            crate::infrastructure::runtime::recipient_surface::open_received_surface(
                workspace.path(),
                Some(&conversation),
                address.item().as_str(),
                &mut state,
                RecipientSurfaceAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            let surface = state.recipient_surface.as_ref().unwrap();
            assert!(matches!(
                &surface.target,
                RecipientSurfaceTarget::Item(opened) if opened == &address
            ));
            assert_eq!(state.focus, FocusState::Chat);
            assert_eq!(input, "unfinished operator text");
            let actions = recipient_surface_actions(&state, &conversation, false);
            assert!(matches!(actions[0], Some(RecipientSurfaceAction::Respond)));
            assert!(
                !actions.contains(&Some(RecipientSurfaceAction::Discuss)),
                "other and unknown semantic types have no Discuss action"
            );
            recipient_surface_key(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
                workspace.path(),
                &conversation,
                &mut input,
                &mut state,
                Some(&tx),
            )
            .await;
            assert!(
                rx.try_recv().is_err(),
                "nonempty input must be preserved rather than overwritten"
            );
            assert_eq!(input, "unfinished operator text");
            assert!(state.recipient_surface.is_some());
            recipient_surface_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                workspace.path(),
                &conversation,
                &mut input,
                &mut state,
                Some(&tx),
            )
            .await;
            assert!(state.recipient_surface.is_none());
            assert_eq!(state.focus, FocusState::Input);
            assert_eq!(input, "unfinished operator text");

            input.clear();
            crate::infrastructure::runtime::recipient_surface::open_received_surface(
                workspace.path(),
                Some(&conversation),
                address.item().as_str(),
                &mut state,
                RecipientSurfaceAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            let actions = recipient_surface_actions(&state, &conversation, false);
            assert!(matches!(actions[0], Some(RecipientSurfaceAction::Respond)));
            recipient_surface_key(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
                workspace.path(),
                &conversation,
                &mut input,
                &mut state,
                Some(&tx),
            )
            .await;
            assert!(rx.try_recv().is_err());
            assert_eq!(input, format!("/team reply {} ", address.item().as_str()));
            assert!(state.recipient_surface.is_none());
            assert_eq!(state.focus, FocusState::Input);
            assert!(state.pinned_peer_compose.is_none());
            input.clear();
        }
    }

    #[test]
    fn ordinary_enter_remains_a_model_turn_while_a_peer_card_is_pending() {
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
        assert!(pending_peer_response(&conversation).is_some());
        let mut input = "ordinary model question".to_owned();
        let mut state = TuiState::new(80, 24);
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, false, &mut input, &mut state, &tx),
            ComposerOutcome::Submit(text) if text == "ordinary model question"
        ));
        assert!(
            matches!(
                take_current_peer_compose(&mut state, &conversation),
                Ok(None)
            ),
            "ordinary model question must not become an implicit FIFO WriteOwn"
        );
        assert!(state.pinned_peer_compose.is_none());
        assert!(rx.try_recv().is_err());
        assert!(pending_peer_response(&conversation).is_some());
    }

    fn paint_received_surface(state: &mut TuiState) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let actions = recipient_surface_actions(state, &Conversation::default(), false);
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_recipient_surface(
                    frame,
                    area,
                    state.recipient_surface.as_mut().unwrap(),
                    &state.recipient_actions,
                    &actions,
                    false,
                    &state.theme,
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
        use crate::adapters::tui::state::{
            RecipientActionState, RecipientSurfaceAccess, RecipientSurfaceTarget,
        };
        use crate::domain::models::SemanticMessageType;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .unwrap();
        let address = append_received_test_item(
            &journal,
            "ri_busy_context",
            SemanticMessageType::Consultation,
            "A real journal-backed consultation body.",
        )
        .await;
        let mut state = TuiState::new(80, 24);
        let mut input = "unsent local composition".to_owned();
        let (tx, mut rx) = mpsc::unbounded_channel();
        crate::infrastructure::runtime::recipient_surface::open_received_surface(
            workspace.path(),
            None,
            address.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        let target = RecipientActionTarget::Item {
            item_id: address.item().as_str().to_owned(),
        };
        recipient_surface_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            workspace.path(),
            &Conversation::default(),
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        assert!(matches!(
            rx.try_recv().unwrap(),
            ClientFrame::DeferRecipientItem { item_id }
                if item_id == address.item().as_str()
        ));
        assert!(action_is_pending(
            &state,
            RecipientAction::Defer,
            &RecipientSurfaceTarget::Item(address.clone()),
            None,
        ));
        recipient_surface_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            workspace.path(),
            &Conversation::default(),
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        assert!(state.feedback_blocks.contains_key("recipient-actions"));
        assert_eq!(state.recipient_actions.len(), 1);
        assert_eq!(input, "unsent local composition");

        crate::infrastructure::runtime::recipient_surface::open_received_surface(
            workspace.path(),
            None,
            address.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        assert!(!paint_received_surface(&mut state).contains("peer-tainted"));
        let notices_before_modal = surface_action_notices(
            &state.recipient_actions,
            &RecipientSurfaceTarget::Item(address.clone()),
            None,
        )
        .count();
        let modal_key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(
            dispatch_recipient_surface_key(
                modal_key,
                true,
                workspace.path(),
                &Conversation::default(),
                &mut input,
                &mut state,
                Some(&tx)
            )
            .await,
            None,
        );
        assert!(
            rx.try_recv().is_err(),
            "modal-owned Context key cannot send a mutation"
        );
        assert!(matches!(
            &state.recipient_surface.as_ref().unwrap().target,
            RecipientSurfaceTarget::Item(opened) if opened == &address
        ));
        assert_eq!(input, "unsent local composition");
        assert_eq!(
            surface_action_notices(
                &state.recipient_actions,
                &RecipientSurfaceTarget::Item(address.clone()),
                None,
            )
            .count(),
            notices_before_modal,
            "the modal-owned key leaves the busy notice intact"
        );
        assert_eq!(
            dispatch_recipient_surface_key(
                modal_key,
                false,
                workspace.path(),
                &Conversation::default(),
                &mut input,
                &mut state,
                Some(&tx)
            )
            .await,
            Some(false),
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            ClientFrame::SetRecipientContext {
                item_id,
                selected: true,
            } if item_id == address.item().as_str()
        ));
        assert!(action_is_pending(
            &state,
            RecipientAction::Defer,
            &RecipientSurfaceTarget::Item(address.clone()),
            None,
        ));
        assert!(action_is_pending(
            &state,
            RecipientAction::ContextSelect,
            &RecipientSurfaceTarget::Item(address.clone()),
            None,
        ));
        assert_eq!(state.recipient_actions.len(), 2);
        assert_eq!(input, "unsent local composition");

        apply_action_result(
            &mut state,
            &mut input,
            RecipientAction::ContextSelect,
            target.clone(),
            RecipientActionOutcome::ContextSelection {
                selected: true,
                changed: true,
            },
        );
        assert!(
            state
                .recipient_context_ids
                .contains(address.item().as_str())
        );
        let surface = state.recipient_surface.as_ref().unwrap();
        assert!(surface.context_selected);
        assert!(
            paint_received_surface(&mut state).contains("peer-tainted"),
            "actual cached header must repaint after correlated selection"
        );
        assert_eq!(input, "unsent local composition");
        assert!(action_is_pending(
            &state,
            RecipientAction::Defer,
            &RecipientSurfaceTarget::Item(address.clone()),
            None,
        ));

        recipient_surface_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            workspace.path(),
            &Conversation::default(),
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        assert!(matches!(
            rx.try_recv().unwrap(),
            ClientFrame::SetRecipientContext {
                item_id,
                selected: false,
            } if item_id == address.item().as_str()
        ));
        apply_action_result(
            &mut state,
            &mut input,
            RecipientAction::ContextRemove,
            target,
            RecipientActionOutcome::ContextSelection {
                selected: false,
                changed: true,
            },
        );
        assert!(
            !state
                .recipient_context_ids
                .contains(address.item().as_str())
        );
        let surface = state.recipient_surface.as_ref().unwrap();
        assert!(!surface.context_selected);
        assert!(
            !paint_received_surface(&mut state).contains("peer-tainted"),
            "actual cached header must repaint after correlated removal"
        );
        assert_eq!(input, "unsent local composition");
        assert!(action_is_pending(
            &state,
            RecipientAction::Defer,
            &RecipientSurfaceTarget::Item(address.clone()),
            None,
        ));
        assert!(matches!(
            state.recipient_actions.get(&(
                RecipientAction::Defer,
                RecipientActionTarget::Item {
                    item_id: address.item().as_str().to_owned(),
                },
            )),
            Some(RecipientActionState::Pending { .. })
        ));

        recipient_surface_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            workspace.path(),
            &Conversation::default(),
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        crate::infrastructure::runtime::recipient_surface::open_received_surface(
            workspace.path(),
            None,
            address.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        assert!(action_is_pending(
            &state,
            RecipientAction::Defer,
            &RecipientSurfaceTarget::Item(address),
            None,
        ));
        assert_eq!(input, "unsent local composition");
    }

    #[tokio::test]
    async fn explicit_legacy_compose_keeps_its_node_and_commands_take_precedence() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let workspace = tempfile::tempdir().unwrap();
        let mut conversation = Conversation {
            messages: vec![peer_row("row-a", "incoming")],
            peer_row_controls: [(
                "row-a".to_owned(),
                peer_control(
                    "node-a",
                    crate::domain::models::PeerRowStage::AwaitingOperator,
                ),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let mut state = TuiState::new(80, 24);
        let mut input = String::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        open_response_surface(
            workspace.path(),
            &conversation,
            "node-a",
            &mut state,
            false,
            true,
        )
        .await
        .unwrap();
        recipient_surface_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            workspace.path(),
            &conversation,
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        let compose = state.pinned_peer_compose.as_ref().unwrap();
        assert_eq!(compose.node_id, "node-a");
        assert!(
            rx.try_recv().is_err(),
            "Respond opens compose without sending"
        );
        conversation
            .messages
            .insert(0, peer_row("row-b", "new head"));
        conversation.peer_row_controls.insert(
            "row-b".to_owned(),
            peer_control("node-b", crate::domain::models::PeerRowStage::Ready),
        );
        assert!(pinned_compose_is_current(&conversation, compose));
        conversation
            .peer_row_controls
            .get_mut("row-a")
            .unwrap()
            .stage = crate::domain::models::PeerRowStage::Settled;
        assert!(!pinned_compose_is_current(&conversation, compose));
        conversation
            .peer_row_controls
            .get_mut("row-a")
            .unwrap()
            .stage = crate::domain::models::PeerRowStage::AwaitingOperator;
        for command in ["/team defer ri_explicit", "/clear"] {
            let mut state = TuiState::new(80, 24);
            state.pinned_peer_compose = Some(crate::adapters::tui::state::PinnedPeerCompose::new(
                "node-a".to_owned(),
                crate::adapters::tui::state::PeerComposeAction::Respond,
            ));
            input = command.to_owned();
            let outcome = attached_composer_key(KeyCode::Enter, false, &mut input, &mut state, &tx);
            if let ComposerOutcome::Submit(text) = outcome {
                assert!(
                    matches!(
                        take_current_peer_compose(&mut state, &conversation),
                        Ok(None)
                    ),
                    "an operator command cannot be submitted as a pinned peer response"
                );
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
            assert!(
                state.pinned_peer_compose.is_none(),
                "an operator command disarms pinned peer composition"
            );
        }
    }

    #[test]
    fn pending_classification_uses_host_controls_even_when_bodies_disagree() {
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
        let pending = pending_peer_response(&conversation).expect("a pending exists");
        assert_eq!(pending.node, "node-a", "FIFO is message order");
        assert_eq!(pending.stage, PeerResponseStage::AwaitingOperator);
        assert_eq!(pending.queued, 2, "settled rows never classify as pending");
    }

    #[test]
    fn no_pending_rows_means_no_card() {
        let conversation = Conversation {
            messages: vec![peer_row("peer-response-node-d", "already sent")],
            ..Default::default()
        };
        assert_eq!(pending_peer_response(&conversation), None);
    }

    #[test]
    fn retract_target_is_the_latest_unretracted_agent_composed_row() {
        let mut latest = peer_row("peer-response-node-b", "second auto reply");
        latest.authorship = crate::domain::models::MessageAuthorship::AgentComposed;
        let mut older = peer_row("peer-response-node-a", "first auto reply");
        older.authorship = crate::domain::models::MessageAuthorship::AgentComposed;
        older.retracted_at_ms = Some(123);
        let conversation = Conversation {
            messages: vec![peer_row("human-1", "operator text"), older, latest.clone()],
            ..Default::default()
        };
        let target = latest_retractable(&conversation).expect("a retract target");
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
        assert!(latest_retractable(&conversation).is_none());
    }

    #[test]
    fn story19_19_review_tool_evidence_has_no_retraction_authority() {
        let mut response = peer_row("peer-response-node-ready", "approved response");
        response.authorship = crate::domain::models::MessageAuthorship::AgentComposed;
        let mut evidence = peer_row("peer-tool-evidence-generated", "tool execution evidence");
        evidence.authorship = crate::domain::models::MessageAuthorship::AgentComposed;
        let conversation = Conversation {
            messages: vec![response.clone(), evidence.clone()],
            ..Default::default()
        };
        assert_eq!(
            latest_retractable(&conversation).map(|row| row.id),
            Some(response.id),
            "displayed peer tool evidence cannot mask an actual response target"
        );
        let conversation = Conversation {
            messages: vec![evidence],
            ..Default::default()
        };
        assert!(latest_retractable(&conversation).is_none());
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
            authorship: Default::default(),
            retracted_at_ms: None,
            history_source: Default::default(),
        }
    }

    fn snapshot_with(transcript: Vec<ChatMessage>, blocked: usize) -> AttachSnapshot {
        AttachSnapshot {
            conversation_id: "conv-test".into(),
            transcript,
            permission_mode: PermissionMode::Normal,
            channels: vec![ChannelKind::Terminal],
            blocked_actions_waiting: blocked,
            pending_consent_cards: 0,
            peer_row_controls: BTreeMap::new(),
            peer_row_revision: 0,
            recipient_context_ids: Vec::new(),
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

    /// Story 19.16c AC3(h) — in an **attached** session a `/team` verb the
    /// intercept table does not name falls through to `driver.submit` and
    /// becomes a model prompt. `trust` and `status` still do; `log`, `board`,
    /// `retract` and (Story 19.17) `send` have explicit local/refusal arms.
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

    /// Story 19.16b AC3(h)1 — `/team board` is REFUSED ALOUD on the attached
    /// rail, ⛔ never a model prompt.
    ///
    /// The board reads every configured A2A peer over this session's own
    /// egress and an attached session holds none, so there is no frame to
    /// send; the one thing that must not happen is the former failure mode —
    /// falling through to `driver.submit` and becoming an LLM turn. Story
    /// 19.17 also intercepts `send`; `trust` and `status` still pass through.
    ///
    /// Mutant → RED: delete the `Board` arm — the line classifies as
    /// `PassThrough` and the operator's board request is typed at the model.
    #[test]
    fn an_attached_team_board_is_refused_aloud_and_never_becomes_a_model_prompt() {
        let AttachedTeamLine::Refused(reason) = attached_team_line("/team board") else {
            panic!("`/team board` must be consumed and explained, never submitted");
        };
        assert!(reason.contains("/team board"), "{reason}");
        assert!(
            reason.contains("non-attached"),
            "the refusal must say where the verb DOES work: {reason}"
        );
        // Control: an unrelated verb keeps its pre-existing fall-through, so
        // the arm above is a named refusal and not a blanket one.
        assert!(matches!(
            attached_team_line("/team status"),
            AttachedTeamLine::PassThrough
        ));
    }

    /// Story 19.16f AC3(c) — `/team retract` is REFUSED ALOUD on the attached
    /// rail with the ratified sentence, ⛔ never a model prompt, and a pasted
    /// leading space does not smuggle it past the table.
    ///
    /// Mutant `M03` → RED: delete the `Retract` arm — the line classifies as
    /// `PassThrough` and a destructive verb is typed at the model.
    #[test]
    fn an_attached_team_retract_is_refused_aloud_and_never_becomes_a_model_prompt() {
        for line in ["/team retract a ri_x", "  /team retract a ri_x"] {
            let AttachedTeamLine::Refused(reason) = attached_team_line(line) else {
                panic!("{line:?} must be consumed and explained, never submitted");
            };
            assert_eq!(
                reason,
                "'/team retract' needs this session's own A2A egress — run it in a \
                 non-attached session."
            );
        }
        // Positive control: `/team ack` keeps its shipped verdict.
        assert!(matches!(
            attached_team_line("/team ack ri_x"),
            AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { .. })
        ));
    }

    #[test]
    fn attached_team_send_and_bare_team_do_not_reach_the_model() {
        let AttachedTeamLine::Refused(reason) = attached_team_line("/team send a,b hi") else {
            panic!("send must be refused aloud on the attached rail");
        };
        assert_eq!(
            reason,
            "'/team send' needs this session's own A2A egress — run it in a non-attached session."
        );
        for input in ["/team", " /team", "/team\tlog"] {
            assert!(
                matches!(attached_team_line(input), AttachedTeamLine::LocalLog(_)),
                "{input:?}"
            );
        }
        assert!(matches!(
            attached_team_line("/teamwork today"),
            AttachedTeamLine::PassThrough
        ));
        assert!(matches!(
            attached_team_line("/team ack ri_x"),
            AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { .. })
        ));
        assert!(matches!(
            attached_team_line("/team board"),
            AttachedTeamLine::Refused(_)
        ));
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
                None,
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
        let mut state = TuiState::new(80, 24);
        state.status = StatusState::Idle;
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
            &mut state,
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

    /// Story 19.16g K08 / M08 — the attached `/team log` is a LOCAL read
    /// (zero frames, never a model turn), presented only by a successful
    /// draw; a read-only client can reach it while every other submission
    /// stays refused without a frame; and the observer never touches a turn.
    #[tokio::test]
    async fn attached_log_is_local_and_streaming_survives() {
        use crate::adapters::tui::handlers::team_command::TEAM_LOG_BLOCK_ID;
        use crate::adapters::tui::state::LogAwareness;
        use crate::infrastructure::transparency_awareness::LogAwarenessObserver;
        use crate::infrastructure::transparency_awareness::test_support::append_rows;
        use crossterm::event::KeyCode;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        fn draw_attached(state: &TuiState) -> Vec<String> {
            let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let mut tab = TabRenderState::default();
            let tools = HashMap::new();
            let mut visible = Vec::new();
            term.draw(|f| {
                visible = chat_pane::render_attached(
                    f,
                    f.area(),
                    &Conversation::default(),
                    &StreamingState::default(),
                    0,
                    true,
                    &state.theme,
                    &mut tab,
                    &tools,
                    &state.feedback_blocks,
                    state.active_feedback_id.as_deref(),
                )
                .visible_feedback_ids;
            })
            .unwrap();
            visible
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
        let mut input = "  /team log".to_owned();
        let ComposerOutcome::LocalLog(args) =
            attached_composer_key(KeyCode::Enter, false, &mut input, &mut state, &tx)
        else {
            panic!("`/team log` must be a local read on the attached rail");
        };
        assert!(input.is_empty());
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
            state.feedback_blocks[TEAM_LOG_BLOCK_ID]
                .message
                .contains("append-only")
        );

        // Painted, but the draw failed: nothing presented.
        let visible = draw_attached(&state);
        assert!(visible.iter().any(|id| id == TEAM_LOG_BLOCK_ID));
        after_attached_draw(&mut state, false, &visible);
        assert_eq!(
            poll(&mut observer, &mut state).await,
            LogAwareness::Unseen(3)
        );
        after_attached_draw(&mut state, true, &visible);
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
        let mut input = "/team log --filter=bogus=term".to_owned();
        let ComposerOutcome::LocalLog(args) =
            attached_composer_key(KeyCode::Enter, false, &mut input, &mut state, &tx)
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
        let mut input = String::new();
        for c in "/team logx".chars() {
            attached_composer_key(KeyCode::Char(c), true, &mut input, &mut state, &tx);
        }
        attached_composer_key(KeyCode::Backspace, true, &mut input, &mut state, &tx);
        assert_eq!(input, "/team log");
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, true, &mut input, &mut state, &tx),
            ComposerOutcome::LocalLog(_)
        ));
        // Read-only attachments may inspect inventory, but item-argument
        // context frames remain writer-only.
        let mut input = "/team context".to_owned();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, true, &mut input, &mut state, &tx),
            ComposerOutcome::FrameSent
        ));
        assert!(input.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientFrame::InspectRecipientContext)
        ));
        for line in ["/team context ri_x", "/team context off ri_x"] {
            let mut input = line.to_owned();
            assert!(matches!(
                attached_composer_key(KeyCode::Enter, true, &mut input, &mut state, &tx),
                ComposerOutcome::Handled
            ));
            assert_eq!(input, line);
            assert!(matches!(
                &state.status,
                StatusState::Flash { message, .. } if message == "read-only — can't send here"
            ));
        }
        assert!(rx.try_recv().is_err(), "item context writes are refused");

        // Writable text still submits; an intercepted verb still frames.
        let mut input = "hello model".to_owned();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, false, &mut input, &mut state, &tx),
            ComposerOutcome::Submit(text) if text == "hello model"
        ));
        let mut input = "/team ack ri_x".to_owned();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, false, &mut input, &mut state, &tx),
            ComposerOutcome::FrameSent
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientFrame::AcknowledgeRecipientItem { .. })
        ));
    }

    /// Story 19.18 `K14` — one intercept row per new verb. `/team reply` is a
    /// daemon frame; the asker's verbs refuse aloud; nothing new falls
    /// through to the model.
    ///
    /// `M24` → RED: delete the `Thread` refusal arm — it falls to
    /// `PassThrough` and becomes a model prompt.
    #[test]
    fn reply_is_a_frame_and_the_askers_verbs_refuse_on_the_attached_rail() {
        match attached_team_line("/team reply ri_x hi  there") {
            AttachedTeamLine::Frame(ClientFrame::ReplyToRecipientItem { item_id, text }) => {
                assert_eq!(item_id, "ri_x");
                assert_eq!(text, "hi  there");
            }
            other => panic!("expected the reply frame, got {other:?}"),
        }
        for (line, verb) in [
            ("/team thread b ri_x", "'/team thread'"),
            ("/team followup b rr_y hi", "'/team followup'"),
        ] {
            let AttachedTeamLine::Refused(reason) = attached_team_line(line) else {
                panic!("{line} must refuse aloud on the attached rail");
            };
            assert!(reason.starts_with(verb), "{reason}");
            assert!(reason.contains("own A2A egress"), "{reason}");
        }
        assert!(matches!(
            attached_team_line("/teamwork"),
            AttachedTeamLine::PassThrough
        ));
        // Positive control: the shipped acknowledgement frame is unchanged.
        assert!(matches!(
            attached_team_line("/team ack ri_x"),
            AttachedTeamLine::Frame(ClientFrame::AcknowledgeRecipientItem { .. })
        ));
    }
    #[test]
    fn local_view_does_not_select_recipient_context() {
        use crossterm::event::KeyCode;

        let mut state = TuiState::new(80, 24);
        let mut input = "/team view ri_view_only".to_owned();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(matches!(
            attached_composer_key(KeyCode::Enter, false, &mut input, &mut state, &tx),
            ComposerOutcome::LocalView(item_id) if item_id == "ri_view_only"
        ));
        assert!(rx.try_recv().is_err(), "View is local display, not consent");
    }
    #[tokio::test]
    async fn scoped_surface_ctrl_c_and_ctrl_d_take_attached_detach_exit() {
        use crate::adapters::tui::state::RecipientSurfaceAccess;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .unwrap();
        let address = append_received_test_item(
            &journal,
            "ri_detach_surface",
            crate::domain::models::SemanticMessageType::Consultation,
            "A received item for scoped detach control.",
        )
        .await;
        let mut state = TuiState::new(80, 24);
        crate::infrastructure::runtime::recipient_surface::open_received_surface(
            workspace.path(),
            None,
            address.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        let mut input = "preserved local input".to_owned();
        let (tx, mut rx) = mpsc::unbounded_channel();
        for chord in ['c', 'd'] {
            assert_eq!(
                dispatch_recipient_surface_key(
                    KeyEvent::new(KeyCode::Char(chord), KeyModifiers::CONTROL),
                    false,
                    workspace.path(),
                    &Conversation::default(),
                    &mut input,
                    &mut state,
                    Some(&tx),
                )
                .await,
                Some(true),
                "attached Ctrl+C/Ctrl+D must reach the real detach exit signal",
            );
        }
        assert_eq!(input, "preserved local input");
        assert!(state.recipient_surface.is_some());
        assert!(rx.try_recv().is_err(), "detach chords are not peer actions");
    }
}
