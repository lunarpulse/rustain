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
    retro_overlay,
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
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
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
    stories: Option<&'a crate::domain::models::StorySummary>,
    status: Option<&'a crate::domain::models::StatusPreview>,
    selected_for: Option<&'a crate::domain::models::SelectionSummary>,
    retro: Option<&'a crate::domain::models::RetroPreview>,
    retro_left_out: Option<&'a crate::domain::models::RetroFieldSet>,
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
        stories: control.stories.as_ref(),
        status: control.status.as_ref(),
        selected_for: control.selected_for.as_ref(),
        retro: control.retro.as_ref(),
        retro_left_out: None,
    })
}

/// Story 19.25 `AC3` — the card as drawn: the pending response plus this
/// client's leave-out overlay for its item.
fn pending_peer_card<'a>(
    conversation: &'a Conversation,
    retro_left_out: &'a HashMap<String, crate::domain::models::RetroFieldSet>,
) -> Option<PendingPeerResponse<'a>> {
    let mut pending = pending_peer_response(conversation)?;
    pending.retro_left_out = pending
        .item_address
        .and_then(|address| retro_left_out.get(address.item().as_str()));
    Some(pending)
}

/// The most recent unretracted automatic response supported by the daemon.
/// Diagnostic/tool-evidence rows have no response identity, and pending response
/// controls remain ineligible even when an edited draft is agent-composed.
fn latest_retractable(conversation: &Conversation) -> Option<ChatMessage> {
    conversation
        .messages
        .iter()
        .rev()
        .find(|message| {
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
    width: usize,
) -> Vec<ratatui::text::Line<'a>> {
    use ratatui::text::{Line, Span};
    let queued = if pending.queued > 0 {
        format!("  [{} more queued]", pending.queued)
    } else {
        String::new()
    };
    let mut lines = Vec::with_capacity(
        3 + usize::from(pending.stories.is_some()) + usize::from(pending.selected_for.is_some()),
    );
    lines.push(Line::from(vec![
        Span::raw(pending.canonical_type),
        Span::raw(" · "),
        Span::raw(pending.sender_label),
    ]));
    if let Some(summary) = pending.stories {
        lines.push(Line::from(summary.0.as_str()));
    }
    if let Some(selection) = pending.selected_for {
        lines.push(Line::from(selection.0.as_str()));
    }
    if let Some(preview) = pending.status {
        use crate::adapters::tui::widgets::chat_pane::word_wrap::wrap_text;
        use crate::domain::models::{NOT_SHARED, NothingBecause};
        let shares = match preview.nothing_because {
            Some(NothingBecause::LevelNone) => {
                "Shares nothing: your sharing level is none".to_owned()
            }
            Some(NothingBecause::NoStoriesHeld) => {
                "Shares nothing: no stories are assigned to you on this host".to_owned()
            }
            None => {
                let mut text = format!(
                    "Shares ({}): ",
                    preview.statement.level.unwrap_or_default().as_str()
                );
                for (index, story) in preview.statement.stories.iter().enumerate() {
                    if index != 0 {
                        text.push_str(" · ");
                    }
                    text.push_str(story.key.as_str());
                    if let Some(state) = story.state {
                        text.push(' ');
                        text.push_str(state.as_str());
                    }
                }
                text
            }
        };
        for text in [shares.as_str(), NOT_SHARED] {
            lines.extend(
                wrap_text(text, width.saturating_sub(2).max(1))
                    .into_iter()
                    .map(|line| Line::from(format!("  {line}"))),
            );
        }
    }
    if let Some(preview) = pending.retro {
        use crate::adapters::tui::widgets::chat_pane::word_wrap::wrap_text;
        // The card's own width, like the status preview: a floor wider than
        // the pane would cut lines the card promises never to truncate.
        let wrap_width = width.saturating_sub(2).max(1);
        for text in crate::adapters::tui::widgets::recipient_surface::retro_preview_lines(
            preview,
            pending.retro_left_out,
        ) {
            lines.extend(
                wrap_text(&text, wrap_width)
                    .into_iter()
                    .map(|line| Line::from(format!("  {line}"))),
            );
        }
    }
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
        // Story 19.22 — verdict offers are item facts; a node surface has none.
        verdict_offer: crate::domain::models::VerdictOffer::None,
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
    if let Some(summary) = &control.stories {
        content.push_str("  ");
        content.push_str(&summary.0);
        content.push('\n');
    }
    if let Some(selection) = &control.selected_for {
        content.push_str("  ");
        content.push_str(&selection.0);
        content.push('\n');
    }
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
            | ClientFrame::ShareRecipientStatus { .. }
            | ClientFrame::ShareRecipientRetro { .. }
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
        .find(|action| matches!(key.code, KeyCode::Char(c) if action.accepts(c)))
    else {
        return false;
    };
    let Some(frame_tx) = write_frames else {
        return false;
    };
    match action {
        RecipientSurfaceAction::Respond
        | RecipientSurfaceAction::Discuss
        // Story 19.22 `AC4(c)` — `[d] Discuss concerns` composes a typed
        // verdict reply; the pending compose is tracked on submit like any
        // `/team reply` line.
        | RecipientSurfaceAction::DiscussConcerns
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
                        let item_id = address.item().as_str();
                        *input = if action == RecipientSurfaceAction::DiscussConcerns {
                            format!("/team reply --discuss {item_id} ")
                        } else {
                            format!("/team reply {item_id} ")
                        };
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
        // Story 19.22 `AC4(c)` — `[y] Accept` queues the verdict frame
        // directly: empty text (the daemon normalizes `Accepted.`) and an
        // Accept verdict, through the same correlated Reply slot as a typed
        // reply.
        RecipientSurfaceAction::Accept => {
            let RecipientSurfaceTarget::Item(address) = &surface.target else {
                return false;
            };
            let frame = ClientFrame::ReplyToRecipientItem {
                item_id: address.item().as_str().to_owned(),
                text: String::new(),
                verdict: Some(crate::adapters::daemon::protocol::ReplyVerdict::Accept),
            };
            queue_recipient_action(state, input, frame, frame_tx, false);
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
        RecipientSurfaceAction::ShareStatus | RecipientSurfaceAction::DeclineStatus => {
            let RecipientSurfaceTarget::Item(address) = &surface.target else {
                return false;
            };
            let frame = ClientFrame::ShareRecipientStatus {
                item_id: address.item().as_str().to_owned(),
                decline: action == RecipientSurfaceAction::DeclineStatus,
            };
            queue_recipient_action(state, input, frame, frame_tx, false);
        }
        // Story 19.25 `AC3` — Share sends the stored overlay as its leave-out
        // list; Decline sends no fields (the daemon refuses decline + fields).
        RecipientSurfaceAction::RetroShare | RecipientSurfaceAction::RetroDecline => {
            let RecipientSurfaceTarget::Item(address) = &surface.target else {
                return false;
            };
            let item_id = address.item().as_str().to_owned();
            let decline = action == RecipientSurfaceAction::RetroDecline;
            let leave_out = if decline {
                Vec::new()
            } else {
                state
                    .retro_left_out
                    .get(&item_id)
                    .map(|set| set.iter().copied().collect())
                    .unwrap_or_default()
            };
            let frame = ClientFrame::ShareRecipientRetro {
                item_id,
                leave_out,
                decline,
            };
            queue_recipient_action(state, input, frame, frame_tx, false);
        }
        // Story 19.25 `AC3` — a digit toggles one field of this client's
        // overlay; the card and the surface re-render from it.
        RecipientSurfaceAction::RetroLeaveOut => {
            let (RecipientSurfaceTarget::Item(address), KeyCode::Char(digit)) =
                (&surface.target, key.code)
            else {
                return false;
            };
            let Some(field) =
                crate::adapters::tui::widgets::recipient_surface::retro_field_for_key(digit)
            else {
                return false;
            };
            let item_id = address.item().as_str().to_owned();
            let set = state.retro_left_out.entry(item_id.clone()).or_default();
            if !set.insert(field) {
                set.remove(&field);
            }
            if set.is_empty() {
                state.retro_left_out.remove(&item_id);
            }
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

/// Story 19.20c: transient writer-only approval state. Consent/consolidation/
/// retract confirmations keep precedence; generic permissions precede proposals.
/// The pinned reader and feedback owner are request ids, never FIFO positions.
#[derive(Default)]
struct AttachedApprovals {
    order: std::collections::VecDeque<crate::domain::models::tool_call::RequestId>,
    pending: HashMap<crate::domain::models::tool_call::RequestId, AttachedApproval>,
    review: Option<crate::domain::models::tool_call::RequestId>,
    feedback: Option<(crate::domain::models::tool_call::RequestId, String)>,
    painted: Option<crate::domain::models::tool_call::RequestId>,
    scroll: usize,
    scroll_max: usize,
    presentation: Option<AttachedApprovalPresentation>,
}

struct AttachedApproval {
    tool: String,
    source: crate::domain::models::tool_call::ApprovalSource,
    risk: crate::domain::models::ToolRisk,
    summary: String,
    assembly: Option<crate::adapters::daemon::protocol::ExactInputAssembly>,
    value: Option<serde_json::Value>,
}

/// Rewrap only on request/view/width/completion/queue changes, not on ticks.
struct AttachedApprovalPresentation {
    id: crate::domain::models::tool_call::RequestId,
    width: u16,
    queued: usize,
    reviewing: bool,
    ready: bool,
    header: Vec<ratatui::text::Line<'static>>,
    body: Vec<ratatui::text::Line<'static>>,
    footer: Vec<ratatui::text::Line<'static>>,
}

impl AttachedApproval {
    fn card(
        &self,
        queue_len: usize,
    ) -> crate::adapters::tui::widgets::permission_prompt::PermissionCard<'_> {
        use crate::adapters::tui::widgets::permission_prompt::{ExactInput, PermissionCard};
        PermissionCard {
            source: &self.source,
            tool_name: &self.tool,
            risk: self.risk,
            input: self.value.as_ref().map_or(
                ExactInput::Receiving {
                    summary: &self.summary,
                },
                ExactInput::Ready,
            ),
            queue_len,
        }
    }
}

impl AttachedApprovals {
    fn shown(&self) -> Option<&crate::domain::models::tool_call::RequestId> {
        self.feedback
            .as_ref()
            .map(|(id, _)| id)
            .or(self.review.as_ref())
            .or(self.order.front())
    }

    fn retire(&mut self, id: &crate::domain::models::tool_call::RequestId) {
        let was_shown = self.shown() == Some(id);
        self.pending.remove(id);
        if self
            .presentation
            .as_ref()
            .is_some_and(|view| &view.id == id)
        {
            self.presentation = None;
        }
        self.order.retain(|queued| queued != id);
        if self.review.as_ref() == Some(id) {
            self.review = None;
        }
        if self.feedback.as_ref().is_some_and(|(owner, _)| owner == id) {
            self.feedback = None;
        }
        if self.painted.as_ref() == Some(id) {
            self.painted = None;
        }
        if was_shown {
            self.scroll = 0;
            self.scroll_max = 0;
        }
    }

    /// All private generic approval frames enter here from the socket arm.
    /// A bad chunk permanently invalidates this transfer, not the decision.
    fn apply(&mut self, frame: DaemonFrame, read_only: bool) -> Option<String> {
        use crate::adapters::daemon::protocol::{ApprovalRefusal, ExactInputAssembly};
        match frame {
            DaemonFrame::ApprovalRequest {
                request_id,
                tool,
                source,
                risk,
                summary,
                input_bytes: Some(bytes),
            } if !read_only => {
                if !self.pending.contains_key(&request_id) {
                    self.order.push_back(request_id.clone());
                    self.pending.insert(
                        request_id,
                        AttachedApproval {
                            tool,
                            source,
                            risk,
                            summary,
                            assembly: Some(ExactInputAssembly::new(bytes)),
                            value: None,
                        },
                    );
                }
            }
            DaemonFrame::ApprovalInputChunk {
                request_id,
                sequence,
                data,
            } if !read_only => {
                if let Some(pending) = self.pending.get_mut(&request_id)
                    && let Some(assembly) = pending.assembly.as_mut()
                    && assembly.push(sequence, &data).is_err()
                {
                    pending.assembly = None;
                }
            }
            DaemonFrame::ApprovalInputComplete { request_id, chunks } if !read_only => {
                if let Some(pending) = self.pending.get_mut(&request_id)
                    && let Some(assembly) = pending.assembly.take()
                {
                    pending.value = assembly.finish(chunks).ok();
                }
            }
            DaemonFrame::ApprovalClosed { request_id } => self.retire(&request_id),
            DaemonFrame::ApprovalRefused { request_id, reason } => {
                if reason == ApprovalRefusal::NoLongerPending {
                    self.retire(&request_id);
                }
                return Some(reason.to_string());
            }
            _ => {}
        }
        None
    }

    fn decide(
        &mut self,
        id: crate::domain::models::tool_call::RequestId,
        outcome: crate::domain::models::ApprovalOutcome,
        tx: &mpsc::UnboundedSender<ClientFrame>,
    ) {
        if tx
            .send(ClientFrame::ApprovalResponse {
                request_id: id.clone(),
                outcome,
            })
            .is_ok()
        {
            self.retire(&id);
        }
    }

    /// Same ownership gate as attached proposals: painted, writer, unmodified
    /// Press, empty composer, and no higher-priority confirmation.
    fn key(
        &mut self,
        key: crossterm::event::KeyEvent,
        blocked: bool,
        input: &str,
        tx: &mpsc::UnboundedSender<ClientFrame>,
    ) -> bool {
        use crate::domain::models::{ApprovalOutcome, ApprovalScope};
        use crossterm::event::{KeyCode, KeyEventKind};
        if blocked || !input.is_empty() {
            return false;
        }
        let Some(id) = self.shown().cloned() else {
            return false;
        };
        if self.painted.as_ref() != Some(&id) {
            return false;
        }
        if key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            return false;
        }
        if key.kind != KeyEventKind::Press {
            return true;
        }
        if let Some((_, buffer)) = self.feedback.as_mut() {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, modifiers) if modifiers.is_empty() => {
                    self.feedback = None;
                }
                (KeyCode::Enter, modifiers) if modifiers.is_empty() => {
                    let feedback = (!buffer.is_empty()).then(|| buffer.clone());
                    self.decide(id, ApprovalOutcome::Reject { feedback }, tx);
                }
                (KeyCode::Backspace, modifiers) if modifiers.is_empty() => {
                    buffer.pop();
                }
                (KeyCode::Char(c), modifiers)
                    if modifiers.is_empty()
                        || modifiers == crossterm::event::KeyModifiers::SHIFT =>
                {
                    buffer.push(c)
                }
                _ => {}
            }
            return true;
        }
        if !key.modifiers.is_empty() {
            return true;
        }
        match key.code {
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(5);
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(5).min(self.scroll_max);
            }
            KeyCode::Esc if self.review.is_some() => {
                self.review = None;
                self.scroll = 0;
            }
            KeyCode::Char('r') => {
                if self
                    .pending
                    .get(&id)
                    .is_some_and(|pending| pending.value.is_some())
                {
                    self.review = Some(id);
                    self.scroll = 0;
                }
            }
            KeyCode::Char('f') => {
                self.feedback = Some((id, String::new()));
            }
            KeyCode::Char('y') => self.decide(id, ApprovalOutcome::Once, tx),
            KeyCode::Char('s') => {
                let tool_name = self.pending[&id].tool.clone();
                self.decide(id, ApprovalOutcome::AlwaysTool { tool_name }, tx);
            }
            KeyCode::Char('a') => {
                let scope = ApprovalScope::Tool(self.pending[&id].tool.clone());
                self.decide(id, ApprovalOutcome::AlwaysAndSave { scope }, tx);
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.decide(id, ApprovalOutcome::Reject { feedback: None }, tx)
            }
            _ => {}
        }
        true
    }

    fn paint(
        &mut self,
        f: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        theme: &crate::adapters::tui::theme::Theme,
    ) -> Option<crate::domain::models::tool_call::RequestId> {
        use crate::adapters::tui::widgets::permission_prompt::{
            paint_rows, permission_review_rows, render_feedback_input_lines,
            render_permission_card_lines, render_permission_review,
        };
        use ratatui::widgets::{Clear, Paragraph};
        let id = self.shown()?.clone();
        let pending = self.pending.get(&id)?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let queued = self.order.len().saturating_sub(1);
        let reviewing = self.review.as_ref() == Some(&id);
        let ready = pending.value.is_some();
        if self.presentation.as_ref().is_none_or(|view| {
            view.id != id
                || view.width != area.width
                || view.queued != queued
                || view.reviewing != reviewing
                || view.ready != ready
        }) {
            let card = pending.card(queued);
            let (header, body, footer) = if reviewing {
                let review = render_permission_review(&card, area.width, theme);
                (review.header, review.body, review.footer)
            } else {
                (
                    Vec::new(),
                    render_permission_card_lines(&card, area.width, theme),
                    Vec::new(),
                )
            };
            self.presentation = Some(AttachedApprovalPresentation {
                id: id.clone(),
                width: area.width,
                queued,
                reviewing,
                ready,
                header,
                body,
                footer,
            });
        }
        let view = self.presentation.as_ref()?;
        if let Some((_, buffer)) = self.feedback.as_ref() {
            crate::adapters::tui::widgets::inline_card::render_bottom_anchored_decision_card(
                f.buffer_mut(),
                render_feedback_input_lines(buffer, theme),
                theme.colors.accent,
                area,
            );
        } else if reviewing {
            let (header_rows, body_rows, footer_rows) =
                permission_review_rows(area.height, view.header.len(), view.footer.len());
            self.scroll_max = view.body.len().saturating_sub(body_rows);
            self.scroll = self.scroll.min(self.scroll_max);
            f.render_widget(Clear, area);
            paint_rows(
                f,
                ratatui::layout::Rect::new(area.x, area.y, area.width, header_rows as u16),
                &view.header,
                0,
            );
            paint_rows(
                f,
                ratatui::layout::Rect::new(
                    area.x,
                    area.y + header_rows as u16,
                    area.width,
                    body_rows as u16,
                ),
                &view.body,
                self.scroll,
            );
            paint_rows(
                f,
                ratatui::layout::Rect::new(
                    area.x,
                    area.bottom().saturating_sub(footer_rows as u16),
                    area.width,
                    footer_rows as u16,
                ),
                &view.footer,
                0,
            );
        } else {
            let height = usize::from(area.height).min(view.body.len());
            self.scroll_max = view.body.len().saturating_sub(height);
            self.scroll = self.scroll.min(self.scroll_max);
            let card_area = ratatui::layout::Rect {
                y: area.bottom().saturating_sub(height as u16),
                height: height as u16,
                ..area
            };
            f.render_widget(Clear, card_area);
            f.render_widget(
                Paragraph::new(
                    view.body
                        .iter()
                        .skip(self.scroll)
                        .take(height)
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                card_area,
            );
        }
        Some(id)
    }
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

fn retained_approval_attach_notice(count: usize, read_only: bool) -> Option<String> {
    (read_only && count > 0).then(|| format!("{count} approval(s) waiting for the writer"))
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
struct AttachedTeamRequests {
    next_id: u64,
    next_send_id: u64,
    pending: std::collections::BTreeMap<String, AttachedTeamRequest>,
    latest_board: Option<String>,
    latest_thread: Option<String>,
    open_retract: Option<String>,
}

impl AttachedTeamRequests {
    fn mint(&mut self) -> String {
        self.next_id += 1;
        format!("attached-team-{}", self.next_id)
    }
}

/// Story 19.21c: own every card key before detach, Ctrl+X or the composer.
fn attached_team_retract_key(
    state: &mut TuiState,
    requests: &mut AttachedTeamRequests,
    key: crossterm::event::KeyEvent,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
) -> bool {
    use crate::adapters::tui::handlers::team_command as handler;
    use crossterm::event::{KeyCode, KeyModifiers};
    let Some(pending) = &state.pending_team_retract else {
        return false;
    };
    let decision = if !key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('y') if pending.armed => Some(true),
            KeyCode::Char('n') | KeyCode::Esc => Some(false),
            _ => None,
        }
    } else {
        None
    };
    if let Some(confirm) = decision {
        handler::resolve_team_retract_card(state, confirm);
        if let Some(request) = requests.open_retract.take() {
            if confirm {
                requests.pending.insert(
                    request.clone(),
                    AttachedTeamRequest::Retract {
                        next_sequence: 0,
                        view: String::new(),
                        view_done: false,
                    },
                );
                handler::show_team_retract(state, handler::TEAM_RETRACT_SENDING.to_owned());
            } else {
                requests.pending.remove(&request);
            }
            if frame_tx
                .send(ClientFrame::TeamRetractDecision {
                    request: request.clone(),
                    confirm,
                })
                .is_err()
            {
                if let Some(pending) = requests.pending.remove(&request) {
                    paint_team_request_failure(state, pending, "could not enqueue command");
                }
            }
        }
    }
    true
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
    input: &mut String,
    state: &mut TuiState,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
) -> ComposerOutcome {
    attached_composer_key_with_team_requests(
        code,
        read_only,
        input,
        state,
        frame_tx,
        &mut AttachedTeamRequests::default(),
    )
}

fn attached_composer_key_with_team_requests(
    code: crossterm::event::KeyCode,
    read_only: bool,
    input: &mut String,
    state: &mut TuiState,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
    team_requests: &mut AttachedTeamRequests,
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
        KeyCode::Enter => {
            let request = team_requests.mint();
            match attached_team_line_for_request(input, request) {
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
                AttachedTeamLine::Frame(frame @ ClientFrame::TeamBoard { .. })
                | AttachedTeamLine::Frame(frame @ ClientFrame::TeamThread { .. })
                | AttachedTeamLine::Frame(frame @ ClientFrame::TeamStatus { .. }) => {
                    state.pinned_peer_compose = None;
                    if enqueue_attached_team_frame(state, team_requests, frame, frame_tx) {
                        input.clear();
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
                        state.pinned_peer_compose = None;
                        if enqueue_attached_team_frame(state, team_requests, frame, frame_tx) {
                            input.clear();
                            return ComposerOutcome::FrameSent;
                        }
                    }
                }
                AttachedTeamLine::Frame(frame) if read_only => {
                    flash(state, "read-only — can't send here".into());
                    let _ = frame;
                }
                AttachedTeamLine::Frame(frame) => {
                    state.pinned_peer_compose = None;
                    if !queue_recipient_action(state, input, frame, frame_tx, true) {
                        return ComposerOutcome::Handled;
                    }
                    return ComposerOutcome::FrameSent;
                }
                AttachedTeamLine::Refused(error) => {
                    input.clear();
                    state.pinned_peer_compose = None;
                    flash(state, error);
                }
                AttachedTeamLine::PassThrough => {
                    if read_only {
                        flash(state, "read-only — can't send here".into());
                    } else {
                        if is_attached_command(input) {
                            state.pinned_peer_compose = None;
                        }
                        return ComposerOutcome::Submit(std::mem::take(input));
                    }
                }
            }
        }
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

fn begin_attached_team_send(
    state: &mut TuiState,
    recipients: &[String],
    message_type: crate::domain::models::SemanticMessageType,
) -> AttachedTeamRequest {
    use crate::adapters::tui::handlers::team_command::{
        TeamSendRow, render_team_send_rows, show_team_send,
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
    show_team_send(state, &block_id, render_team_send_rows(&rows));
    state.team_send_blocks.insert(block_id.clone(), rows);
    AttachedTeamRequest::Send {
        block_id,
        action: None,
        message_type,
    }
}

fn attached_proposal_higher_confirmation(
    state: &TuiState,
    consent: bool,
    consolidation: bool,
    retract: bool,
) -> bool {
    consent
        || consolidation
        || retract
        || state.pending_team_retract.is_some()
        || crate::adapters::tui::app::proposal_higher_confirmation_active(state)
}

fn attached_proposal_pending(
    state: &TuiState,
) -> Option<&Arc<crate::domain::models::TeamProposal>> {
    state
        .team_proposal_conversation_id
        .as_ref()
        .and_then(|id| state.pending_team_proposals.get(id))
}

/// The pending recipient action retains the visible composition after taking
/// the local buffer. Display and decision guards must consult the same text.
fn attached_display_input<'a>(state: &'a TuiState, input: &'a str) -> &'a str {
    crate::adapters::tui::handlers::recipient_actions::pending_composition(state).unwrap_or(input)
}

fn scroll_attached_transcript(
    earlier: bool,
    rows: u16,
    streaming: bool,
    offset: &mut usize,
    auto_scroll: &mut bool,
) {
    let readable = rows.saturating_sub(u16::from(streaming)).max(1);
    let step = usize::from(readable).min(5);
    if earlier {
        *offset = offset.saturating_add(step);
        *auto_scroll = false;
    } else {
        *offset = offset.saturating_sub(step);
        if *offset == 0 {
            *auto_scroll = true;
        }
    }
}

/// Nearest production-path key seam: called before recipient/composer dispatch.
/// Only a painted snapshot can decide; a nonempty composer owns its letters.
#[allow(clippy::too_many_arguments)] // Attached loop owns separate composer/tab/socket state.
fn attached_proposal_key(
    key: crossterm::event::KeyEvent,
    higher_confirmation: bool,
    read_only: bool,
    input: &str,
    state: &mut TuiState,
    tab: &mut TabRenderState,
    requests: &mut AttachedTeamRequests,
    frame_tx: &mpsc::UnboundedSender<ClientFrame>,
    scroll_offset: &mut usize,
    auto_scroll: &mut bool,
) -> bool {
    use crate::domain::models::TeamProposalDecision;
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if higher_confirmation
        || read_only
        || !matches!(state.focus, FocusState::Chat | FocusState::Input)
    {
        return false;
    }
    let Some(proposal) = attached_proposal_pending(state).cloned() else {
        return false;
    };
    if !state
        .team_proposal_visible
        .as_ref()
        .is_some_and(|visible| Arc::ptr_eq(visible, &proposal))
    {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    if key.code == KeyCode::Esc {
        return true; // Keep pending, rather than detaching the approving surface.
    }
    if !attached_display_input(state, input).is_empty() {
        return false;
    }
    if key.code == KeyCode::Enter {
        return true; // Enter is never a proposal decision.
    }
    if state.team_proposal_scroll_max > 0
        && key.kind == KeyEventKind::Press
        && key.modifiers.is_empty()
        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
    {
        let reviewing = tab
            .team_proposal_reviews
            .values()
            .any(|review| Arc::ptr_eq(review, &proposal));
        let at_boundary = if key.code == KeyCode::PageUp {
            state.team_proposal_scroll == 0
        } else {
            state.team_proposal_scroll == state.team_proposal_scroll_max
        };
        if reviewing && at_boundary {
            return false;
        }
        let step = state.team_proposal_viewport_rows.saturating_sub(1).max(1);
        state.team_proposal_scroll = if key.code == KeyCode::PageUp {
            state.team_proposal_scroll.saturating_sub(step)
        } else {
            state
                .team_proposal_scroll
                .saturating_add(step)
                .min(state.team_proposal_scroll_max)
        };
        state.team_proposal_footer_visible = false;
        state.needs_redraw = true;
        return true;
    }
    if !matches!(key.code, KeyCode::Char('y' | 'r' | 'n')) {
        return false;
    }
    if key.kind != KeyEventKind::Press || !key.modifiers.is_empty() {
        return true;
    }
    if key.code == KeyCode::Char('r') {
        state.team_proposal_scroll = 0;
        state.team_proposal_scroll_max = 0;
        state.team_proposal_footer_visible = false;
        *scroll_offset = 0;
        *auto_scroll = true;
        if let Some(surface) = state.recipient_surface.take() {
            state.focus = surface.prior_focus;
            crate::adapters::tui::handlers::recipient_actions::refresh_action_notices(state, false);
        }
        let id = format!("team-proposal-review-{}", proposal.id);
        crate::adapters::tui::handlers::team_command::show_team_send(
            state,
            &id,
            format!(
                "Proposed message (not sent) · {}",
                crate::domain::models::semantic_message_type_metadata(proposal.message_type).token
            ),
        );
        tab.team_proposal_reviews.insert(id, proposal);
        state.needs_redraw = true;
        return true;
    }
    if key.code == KeyCode::Char('y') && !state.team_proposal_footer_visible {
        return true;
    }
    let decision = if key.code == KeyCode::Char('y') {
        TeamProposalDecision::Send
    } else {
        TeamProposalDecision::Dismiss
    };
    let frame = ClientFrame::TeamProposalDecision {
        proposal: proposal.id.clone(),
        decision,
    };
    let queued = if decision == TeamProposalDecision::Send {
        enqueue_attached_team_frame(state, requests, frame, frame_tx)
    } else {
        frame_tx.send(frame).is_ok()
    };
    if queued {
        state
            .pending_team_proposals
            .remove(&proposal.conversation_id);
        state.team_proposal_visible = None;
        if decision == TeamProposalDecision::Dismiss {
            crate::adapters::tui::handlers::team_command::show_team_send(
                state,
                "team-proposal-status",
                "Proposal dismissed. Nothing was sent.".into(),
            );
        }
    } else {
        crate::adapters::tui::handlers::team_command::show_team_send(
            state,
            "team-proposal-status",
            "Attach socket closed — the proposal decision was not sent.".into(),
        );
    }
    state.needs_redraw = true;
    true
}

#[allow(clippy::too_many_arguments)] // Match the split attached render state without copying it.
fn prepare_attached_proposal(
    state: &TuiState,
    tab: &TabRenderState,
    input: &str,
    higher_confirmation: bool,
    read_only: bool,
    layout: &mut crate::adapters::tui::layout::AppLayout,
    area: ratatui::layout::Rect,
) -> Option<crate::adapters::tui::widgets::team_proposal_card::CardPaint> {
    let proposal = (!higher_confirmation && !read_only)
        .then(|| attached_proposal_pending(state).cloned())
        .flatten();
    let reviewing = proposal.as_ref().is_some_and(|pending| {
        tab.team_proposal_reviews
            .values()
            .any(|review| Arc::ptr_eq(review, pending))
    });
    crate::adapters::tui::widgets::team_proposal_card::CardView::attached(
        proposal,
        attached_display_input(state, input).is_empty(),
        reviewing,
    )
    .with_scroll(state.team_proposal_scroll)
    .prepare(layout, area, &state.theme)
}

/// Consume private DTOs explicitly; these never enter the public RawEvent bus.
fn apply_attached_proposal_frame(
    state: &mut TuiState,
    tab: &mut TabRenderState,
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
            // The authenticated socket is the conversation binding. A reset's
            // withdrawal arrives before any proposal from its replacement turn.
            state.team_proposal_conversation_id = Some(conversation_id.to_owned());
            state.pending_team_proposals.insert(
                conversation_id.to_owned(),
                Arc::new(crate::domain::models::TeamProposal {
                    id: proposal,
                    conversation_id: conversation_id.to_owned(),
                    recipients,
                    message_type,
                    body,
                }),
            );
            state.team_proposal_visible = None;
            state.team_proposal_scroll = 0;
            state.team_proposal_scroll_max = 0;
        }
        DaemonFrame::TeamProposalClosed { proposal, outcome } => {
            state
                .pending_team_proposals
                .retain(|_, pending| pending.id != proposal);
            if state
                .team_proposal_visible
                .as_ref()
                .is_some_and(|pending| pending.id == proposal)
            {
                state.team_proposal_visible = None;
                state.team_proposal_scroll = 0;
                state.team_proposal_scroll_max = 0;
            }
            if outcome == TeamProposalCloseOutcome::Withdrawn {
                tab.team_proposal_reviews
                    .retain(|_, pending| pending.id != proposal);
            }
            if let Some(AttachedTeamRequest::Send { block_id, .. }) =
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
    use crate::domain::models::TeamProposalDecision;

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
        ClientFrame::TeamThread { request, .. } => {
            requests
                .pending
                .retain(|_, pending| !matches!(pending, AttachedTeamRequest::Thread { .. }));
            requests.latest_thread = Some(request.clone());
            crate::adapters::tui::handlers::team_command::show_team_thread(
                state,
                crate::adapters::tui::handlers::team_command::TEAM_THREAD_READING.to_owned(),
            );
            (
                request.clone(),
                AttachedTeamRequest::Thread {
                    next_sequence: 0,
                    view: String::new(),
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
        ClientFrame::TeamProposalDecision {
            proposal,
            decision: TeamProposalDecision::Send,
        } => {
            let Some(snapshot) = attached_proposal_pending(state)
                .filter(|snapshot| snapshot.id == *proposal)
                .cloned()
            else {
                return false;
            };
            (
                proposal.clone(),
                begin_attached_team_send(state, &snapshot.recipients, snapshot.message_type),
            )
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
        AttachedTeamRequest::Thread { .. } => {
            crate::adapters::tui::handlers::team_command::show_team_thread(state, text)
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
        render_team_send_rows, show_team_board, show_team_send, show_team_status,
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
                .map(|rows| render_team_send_rows(rows));
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
            if !settled {
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
                } if chunks == next_sequence
                    && requests.latest_thread.as_deref() == Some(request.as_str()) =>
                {
                    crate::adapters::tui::handlers::team_command::show_team_thread(state, view)
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
                    let overlay = matches!(
                        state.focus,
                        FocusState::Overlay(
                            crate::domain::models::visual::OverlayType::Confirmation(_)
                        )
                    );
                    if confirmation_busy
                        || overlay
                        || state.pending_team_retract.is_some()
                        || requests.open_retract.is_some()
                    {
                        requests.pending.remove(&request);
                        let _ = frame_tx.send(ClientFrame::TeamRetractDecision {
                            request,
                            confirm: false,
                        });
                        handler::show_team_retract(state,
                            "Another confirmation is already awaiting your answer. Nothing was sent for this one.".into());
                    } else {
                        handler::open_team_retract_card(
                            state,
                            "daemon-attachment",
                            crate::domain::events::TeamRetractPreview::Card(
                                crate::domain::events::TeamRetractCard {
                                    peer,
                                    item_id,
                                    task: None,
                                    body,
                                    armed,
                                },
                            ),
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
        AttachedTeamRequest::Thread { .. } => {
            if requests.latest_thread.as_deref() == Some(request.as_str()) {
                crate::adapters::tui::handlers::team_command::show_team_thread(state, message);
            }
        }
        AttachedTeamRequest::RetractPreview | AttachedTeamRequest::Retract { .. } => {
            if requests.open_retract.as_deref() == Some(request.as_str()) {
                requests.open_retract = None;
                crate::adapters::tui::handlers::team_command::resolve_team_retract_card(
                    state, false,
                );
            }
            crate::adapters::tui::handlers::team_command::show_team_retract(state, message);
        }
        AttachedTeamRequest::Status | AttachedTeamRequest::Untrust => {
            crate::adapters::tui::handlers::team_command::show_team_status(state, message)
        }
    }
    true
}
fn append_attached_paste(input: &mut String, pasted: &str) {
    input.push_str(pasted);
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
    let mut tool_inputs = AttachedToolInputs {
        reasons: snapshot
            .withheld_tool_inputs
            .iter()
            .map(|entry| (entry.tool_call_id.clone(), entry.withheld))
            .collect(),
        ..Default::default()
    };
    if let Some(notice) = retained_approval_attach_notice(snapshot.retained_approvals, read_only) {
        conversation.messages.push(system_message(notice));
    }
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
    let mut attached_team_requests = AttachedTeamRequests::default();
    let mut approvals = AttachedApprovals::default();

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
        let mut visible_proposal = None;
        let mut visible_approval = None;
        // Story 19.25 `AC3` — a card that left takes its leave-out overlay.
        crate::adapters::tui::handlers::recipient_actions::prune_retro_left_out(
            &mut state,
            &conversation,
        );
        let drawn = term.draw(|f| {
            let area = f.area();
            let Some(mut layout) = crate::adapters::tui::layout::compute_layout(
                area,
                &state.theme,
                attached_display_input(&state, &input),
                1,
                false,
                DensityMode::Focus,
            ) else {
                return;
            };
            let proposal_higher = attached_proposal_higher_confirmation(
                &state, !pending_consent_approvals.is_empty(),
                pending_consolidation_card.is_some(), retract_confirm.is_some(),
            ) || (!read_only && !approvals.pending.is_empty());
            let proposal_card = prepare_attached_proposal(
                &state, &tab_render_state, &input, proposal_higher, read_only, &mut layout, area,
            );
            let peer_card = if state.recipient_surface.is_none()
                && retract_confirm.is_none()
                && pending_consolidation_card.is_none()
                && pending_consent_approvals.is_empty()
                && attached_proposal_pending(&state).is_none()
                && approvals.pending.is_empty()
            {
                pending_peer_card(&conversation, &state.retro_left_out).map(|pending| {
                    peer_response_card_lines(
                        &pending,
                        read_only,
                        usize::from(layout.chat_pane.width.saturating_sub(2)),
                    )
                })
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
                &tool_inputs.reasons,
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
            let display_input = attached_display_input(&state, &input);
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
            let retro_left_out = &state.retro_left_out;
            let retro = state
                .recipient_surface
                .as_ref()
                .and_then(|surface| retro_overlay(&conversation, surface, retro_left_out));
            if let Some(surface) = state.recipient_surface.as_mut() {
                render_recipient_surface(f, layout.chat_pane, surface, &state.recipient_actions,
                    &actions, retro, read_only, &state.theme);
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
            // Story 19.21c: the retract card owns every key while open, so it paints last.
            if let Some(pending) = &state.pending_team_retract {
                render_bottom_anchored_decision_card(f.buffer_mut(),
                    crate::adapters::tui::widgets::team_retract_prompt::render_team_retract_lines(
                        pending, &state.theme, layout.chat_pane.width),
                    state.theme.colors.accent, layout.chat_pane);
            }
            state.team_proposal_scroll_max = proposal_card.as_ref().map_or(0, |card| card.scroll_max());
            state.team_proposal_scroll = state.team_proposal_scroll.min(state.team_proposal_scroll_max);
            state.team_proposal_viewport_rows = proposal_card.as_ref().map_or(0, |card| card.viewport_rows());
            state.team_proposal_footer_visible = proposal_card.as_ref().is_some_and(|card| card.footer_visible());
            visible_proposal = crate::adapters::tui::widgets::team_proposal_card::render(
                proposal_card, f.buffer_mut(), &state.theme,
            );
            // Inline cards can leave fewer than five rows; paging must not skip visible content.
            state.viewport_height = transcript_area.height;
            let approval_higher = attached_proposal_higher_confirmation(
                &state, !pending_consent_approvals.is_empty(),
                pending_consolidation_card.is_some(), retract_confirm.is_some(),
            );
            if !read_only && !approval_higher
                && matches!(state.focus, FocusState::Chat | FocusState::Input)
                && attached_display_input(&state, &input).is_empty()
            {
                visible_approval = approvals.paint(f, layout.chat_pane, &state.theme);
            }
        });
        after_attached_draw(&mut state, drawn.is_ok(), &visible_feedback_ids);
        state.team_proposal_visible = drawn.is_ok().then_some(visible_proposal).flatten();
        approvals.painted = drawn.is_ok().then_some(visible_approval).flatten();

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        if attached_team_retract_key(&mut state, &mut attached_team_requests, key, &frame_tx) {
                            continue;
                        }
                        let approval_higher = attached_proposal_higher_confirmation(
                            &state, !pending_consent_approvals.is_empty(),
                            pending_consolidation_card.is_some(), retract_confirm.is_some(),
                        );
                        if approvals.key(
                            key, read_only || approval_higher
                                || !matches!(state.focus, FocusState::Chat | FocusState::Input),
                            attached_display_input(&state, &input), &frame_tx,
                        ) {
                            state.needs_redraw = true;
                            continue;
                        }
                        let proposal_higher = approval_higher
                            || (!read_only && !approvals.pending.is_empty());
                        if attached_proposal_key(
                            key, proposal_higher, read_only, &input, &mut state,
                            &mut tab_render_state, &mut attached_team_requests, &frame_tx,
                            &mut scroll_offset, &mut auto_scroll,
                        ) {
                            continue;
                        }
                        let higher_confirmation = proposal_higher
                            || attached_proposal_pending(&state).is_some();
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
                                let text = match attached_composer_key_with_team_requests(
                                    key.code,
                                    read_only,
                                    &mut input,
                                    &mut state,
                                    &frame_tx,
                                    &mut attached_team_requests,
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
                            (KeyCode::PageUp | KeyCode::PageDown, _) => {
                                scroll_attached_transcript(
                                    key.code == KeyCode::PageUp,
                                    state.viewport_height,
                                    streaming.is_streaming,
                                    &mut scroll_offset,
                                    &mut auto_scroll,
                                );
                            }
                            (KeyCode::Char(_), m) if !m.contains(KeyModifiers::CONTROL) => {
                                if state.recipient_surface.is_some() {
                                    continue;
                                }
                                // Story 19.16g: read-only composers edit too;
                                // only submission is refused.
                                attached_composer_key_with_team_requests(
                                    key.code,
                                    read_only,
                                    &mut input,
                                    &mut state,
                                    &frame_tx,
                                    &mut attached_team_requests,
                                );
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        if state.recipient_surface.is_none()
                            && crate::adapters::tui::handlers::recipient_actions::pending_composition(&state).is_none()
                            && pending_consent_approvals.is_empty()
                            && approvals.pending.is_empty()
                            && pending_consolidation_card.is_none()
                            && retract_confirm.is_none()
                            && state.pending_team_retract.is_none()
                        {
                            append_attached_paste(&mut input, &text);
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
                            tool_inputs.apply(
                                raw,
                                None,
                                &mut conversation,
                                &mut streaming,
                                &mut reducer,
                                &mut state,
                                &mut permission_mode,
                                &clock,
                            );
                        }
                    Ok(Some(DaemonFrame::ToolEventWithheld { event, withheld })) => {
                        tool_inputs.apply(
                            event, Some(withheld), &mut conversation, &mut streaming, &mut reducer,
                            &mut state, &mut permission_mode, &clock,
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
                    Ok(Some(DaemonFrame::FrameTooLarge { frame, bytes })) => {
                        conversation.messages.push(system_message(
                            crate::adapters::daemon::protocol::frame_too_large_notice(&frame, bytes),
                        ));
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
                        summary,
                        ..
                    })) if tool == "a2a/sender-consent" => {
                        pending_consent_approvals.push_back(PendingConsentApproval {
                            request_id,
                            tool,
                            input_preview: summary,
                        });
                        state.needs_redraw = true;
                    }
                    Ok(Some(frame @ DaemonFrame::ApprovalRequest { .. }))
                    | Ok(Some(frame @ DaemonFrame::ApprovalInputChunk { .. }))
                    | Ok(Some(frame @ DaemonFrame::ApprovalInputComplete { .. }))
                    | Ok(Some(frame @ DaemonFrame::ApprovalClosed { .. }))
                    | Ok(Some(frame @ DaemonFrame::ApprovalRefused { .. })) => {
                        if let Some(notice) = approvals.apply(frame, read_only) {
                            conversation.messages.push(system_message(notice));
                        }
                        state.needs_redraw = true;
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
                    Ok(Some(frame @ DaemonFrame::TeamProposal { .. }))
                    | Ok(Some(frame @ DaemonFrame::TeamProposalClosed { .. })) => {
                        apply_attached_proposal_frame(
                            &mut state, &mut tab_render_state, &mut attached_team_requests,
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
                        let confirmation_busy = !pending_consent_approvals.is_empty()
                            || pending_consolidation_token.is_some() || retract_confirm.is_some()
                            || attached_proposal_pending(&state).is_some() || !approvals.pending.is_empty();
                        let _ = apply_attached_team_frame(
                            &mut state, &mut attached_team_requests, frame, confirmation_busy, &frame_tx,
                        );
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
                                &mut state, &mut input, action, target, outcome,
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

    // Supporting seams; signed socket/PTY K02 evidence is run after integration.
    mod exact_attached_approval {
        use super::*;
        use crate::domain::models::tool_call::{ApprovalSource, RequestId};
        use crate::domain::models::{ApprovalOutcome, ToolRisk};
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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
        fn chunk(state: &mut AttachedApprovals, id: &str, sequence: u32, data: &str) {
            state.apply(
                DaemonFrame::ApprovalInputChunk {
                    request_id: RequestId(id.into()),
                    sequence,
                    data: data.into(),
                },
                false,
            );
        }
        fn complete(state: &mut AttachedApprovals, id: &str, chunks: u32) {
            state.apply(
                DaemonFrame::ApprovalInputComplete {
                    request_id: RequestId(id.into()),
                    chunks,
                },
                false,
            );
        }
        fn paint(state: &mut AttachedApprovals) -> String {
            paint_at(state, 60, 16)
        }
        fn paint_at(state: &mut AttachedApprovals, width: u16, height: u16) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let theme = crate::adapters::tui::theme::Theme::dark();
            let mut painted = None;
            terminal
                .draw(|f| {
                    painted = state.paint(f, f.area(), &theme);
                })
                .unwrap();
            state.painted = painted;
            let buffer = terminal.backend().buffer();
            (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        fn key(code: KeyCode) -> KeyEvent {
            KeyEvent::new(code, KeyModifiers::NONE)
        }

        #[test]
        fn review_scroll_resize_cache_and_delimiters_keep_tail_reachable() {
            let mut state = AttachedApprovals::default();
            let input = serde_json::json!({"command": format!("{}\n[y] fake control\n\u{1b}[31mTAIL-CANARY", "αβ ".repeat(300))});
            let data = serde_json::to_string(&input).unwrap();
            state.apply(request("a", &data), false);
            chunk(&mut state, "a", 0, &data);
            complete(&mut state, "a", 1);
            let (tx, mut rx) = mpsc::unbounded_channel();
            paint(&mut state);
            state.key(key(KeyCode::Char('r')), false, "", &tx);
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                paint_at(&mut state, width, height);
                let cached = state.presentation.as_ref().unwrap().body.as_ptr();
                paint_at(&mut state, width, height);
                assert_eq!(cached, state.presentation.as_ref().unwrap().body.as_ptr());
                for row in &state.presentation.as_ref().unwrap().body {
                    assert!(row.to_string().starts_with("│ "));
                    assert!(!row.to_string().contains('\u{1b}'));
                }
                while state.scroll < state.scroll_max {
                    state.key(key(KeyCode::PageDown), false, "", &tx);
                }
                assert!(paint_at(&mut state, width, height).contains("TAIL-CANARY"));
                assert!(rx.try_recv().is_err());
            }
            assert_eq!(
                state.pending[&RequestId("a".into())].value.as_ref(),
                Some(&input)
            );
        }

        #[test]
        fn long_tool_and_source_metadata_leave_review_and_decisions_reachable() {
            let mut state = AttachedApprovals::default();
            let input = serde_json::json!({
                "first": "METADATA-INPUT-HEAD",
                "last": format!("{}\nMETADATA-INPUT-TAIL", "exact ".repeat(250)),
            });
            let data = serde_json::to_string(&input).unwrap();
            let mut frame = request("long-metadata", &data);
            if let DaemonFrame::ApprovalRequest { tool, source, .. } = &mut frame {
                *tool = format!("mcp__{}__{}", "server".repeat(350), "operation".repeat(250));
                *source = ApprovalSource::ForegroundSubagent {
                    conversation_id: "c".into(),
                    parent_tool_call_id: "parent".into(),
                    subagent_type: "source".repeat(400),
                };
            }
            state.apply(frame, false);
            chunk(&mut state, "long-metadata", 0, &data);
            complete(&mut state, "long-metadata", 1);
            let (tx, mut rx) = mpsc::unbounded_channel();
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                // Use the real attached chat allocation, not the whole terminal.
                let theme = crate::adapters::tui::theme::Theme::dark();
                let layout = crate::adapters::tui::layout::compute_layout(
                    ratatui::layout::Rect::new(0, 0, width, height),
                    &theme,
                    "",
                    1,
                    false,
                    DensityMode::Focus,
                )
                .unwrap();
                let pane_height = layout.chat_pane.height;
                state.review = None;
                state.scroll = 0;
                let card = paint_at(&mut state, width, pane_height);
                assert!(card.contains("[r] Review"), "{card}");
                assert!(card.contains("[y] Allow"), "{card}");
                assert!(state.key(key(KeyCode::Char('r')), false, "", &tx));
                let mut painted = String::new();
                loop {
                    let page = paint_at(&mut state, width, pane_height);
                    assert!(page.contains("[y] Allow"), "{page}");
                    assert!(page.contains("[n] Deny"), "{page}");
                    assert!(page.contains("[Esc] Back"), "{page}");
                    painted.push_str(&page);
                    if state.scroll == state.scroll_max {
                        break;
                    }
                    let before = state.scroll;
                    assert!(state.key(key(KeyCode::PageDown), false, "", &tx));
                    assert!(state.scroll > before);
                }
                assert!(painted.contains("METADATA-INPUT-HEAD"));
                assert!(painted.contains("METADATA-INPUT-TAIL"));
                assert_eq!(
                    state.pending[&RequestId("long-metadata".into())]
                        .value
                        .as_ref(),
                    Some(&input)
                );
                assert!(rx.try_recv().is_err(), "scrolling is not a decision");
            }
            assert!(state.key(key(KeyCode::Char('n')), false, "", &tx));
            assert!(matches!(
                rx.try_recv().unwrap(),
                ClientFrame::ApprovalResponse { request_id, outcome: ApprovalOutcome::Reject { feedback: None } }
                    if request_id == RequestId("long-metadata".into())
            ));
        }

        #[test]
        fn rich_exact_receiving_until_valid_complete() {
            let mut state = AttachedApprovals::default();
            let data = r#"{"command":"unique exact tail"}"#;
            state.apply(request("a", data), false);
            let (tx, mut rx) = mpsc::unbounded_channel();
            assert!(paint(&mut state).contains("receiving exact input…"));
            state.key(key(KeyCode::Char('r')), false, "", &tx);
            assert!(state.review.is_none());
            chunk(&mut state, "a", 0, data);
            assert!(state.pending[&RequestId("a".into())].value.is_none());
            assert!(paint(&mut state).contains("receiving exact input…"));
            complete(&mut state, "a", 1);
            assert_eq!(
                state.pending[&RequestId("a".into())].value,
                Some(serde_json::json!({"command": "unique exact tail"}))
            );
            assert!(paint(&mut state).contains("unique exact tail"));
            state.key(key(KeyCode::Char('r')), false, "", &tx);
            assert!(paint(&mut state).contains("Exact tool input (not run yet)"));
            assert!(rx.try_recv().is_err());
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
                let mut state = AttachedApprovals::default();
                state.apply(request("a", data), false);
                match fault {
                    "partial" => chunk(&mut state, "a", 0, &data[..5]),
                    "duplicate" => {
                        chunk(&mut state, "a", 0, data);
                        chunk(&mut state, "a", 0, data);
                    }
                    "out-of-order" => {
                        chunk(&mut state, "a", 1, data);
                        chunk(&mut state, "a", 0, data);
                    }
                    "overflow" => chunk(&mut state, "a", 0, &format!("{data}x")),
                    "wrong-request" => chunk(&mut state, "b", 0, data),
                    "wrong-count" => chunk(&mut state, "a", 0, data),
                    _ => unreachable!(),
                }
                complete(&mut state, "a", if fault == "wrong-count" { 2 } else { 1 });
                assert!(
                    state.pending[&RequestId("a".into())].value.is_none(),
                    "{fault}"
                );
                assert!(
                    paint(&mut state).contains("receiving exact input…"),
                    "{fault}"
                );
            }
        }

        #[test]
        fn review_pins_request_while_another_queued_and_decides_once() {
            let mut state = AttachedApprovals::default();
            let a = r#"{"command":"first exact input"}"#;
            let b = r#"{"command":"second secret input"}"#;
            state.apply(request("a", a), false);
            chunk(&mut state, "a", 0, a);
            complete(&mut state, "a", 1);
            let (tx, mut rx) = mpsc::unbounded_channel();
            paint(&mut state);
            state.key(key(KeyCode::Char('r')), false, "", &tx);
            state.apply(request("b", b), false);
            chunk(&mut state, "b", 0, b);
            complete(&mut state, "b", 1);
            state.order.swap(0, 1);
            let text = paint(&mut state);
            assert!(text.contains("first exact input") && !text.contains("second secret input"));
            state.key(key(KeyCode::Char('y')), false, "", &tx);
            assert!(
                matches!(rx.try_recv().unwrap(), ClientFrame::ApprovalResponse {
                request_id, outcome: ApprovalOutcome::Once,
            } if request_id.0 == "a")
            );
            assert!(!state.key(key(KeyCode::Char('y')), false, "", &tx));
            assert!(rx.try_recv().is_err());
            assert!(state.pending.contains_key(&RequestId("b".into())));
        }

        #[test]
        fn closed_review_and_feedback_retire_and_ignore_late_parts() {
            let mut state = AttachedApprovals::default();
            let data = r#"{"command":"exact tail"}"#;
            state.apply(request("a", data), false);
            chunk(&mut state, "a", 0, data);
            complete(&mut state, "a", 1);
            let (tx, mut rx) = mpsc::unbounded_channel();
            paint(&mut state);
            state.key(key(KeyCode::Char('r')), false, "", &tx);
            paint(&mut state);
            state.key(key(KeyCode::Char('f')), false, "", &tx);
            state.apply(
                DaemonFrame::ApprovalClosed {
                    request_id: RequestId("a".into()),
                },
                false,
            );
            chunk(&mut state, "a", 0, data);
            complete(&mut state, "a", 1);
            assert!(!state.key(key(KeyCode::Char('y')), false, "", &tx));
            assert!(state.review.is_none() && state.feedback.is_none());
            assert!(state.pending.is_empty() && state.order.is_empty() && state.painted.is_none());
            assert!(rx.try_recv().is_err());
        }

        #[test]
        fn only_writer_painted_empty_composer_unmodified_press_decides() {
            let data = r#"{"command":"ok"}"#;
            let mut state = AttachedApprovals::default();
            state.apply(request("a", data), false);
            let (tx, mut rx) = mpsc::unbounded_channel();
            assert!(!state.key(key(KeyCode::Char('y')), false, "", &tx));
            paint(&mut state);
            assert!(!state.key(key(KeyCode::Char('y')), true, "", &tx));
            assert!(!state.key(key(KeyCode::Char('y')), false, "draft", &tx));
            for modifiers in [
                KeyModifiers::SHIFT,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL,
            ] {
                state.key(KeyEvent::new(KeyCode::Char('y'), modifiers), false, "", &tx);
            }
            for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                state.key(
                    KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, kind),
                    false,
                    "",
                    &tx,
                );
            }
            assert!(rx.try_recv().is_err());
            state.key(key(KeyCode::Char('s')), false, "", &tx);
            assert!(
                matches!(rx.try_recv().unwrap(), ClientFrame::ApprovalResponse {
                request_id, outcome: ApprovalOutcome::AlwaysTool { tool_name },
            } if request_id.0 == "a" && tool_name == "Bash")
            );
            assert!(rx.try_recv().is_err());
        }

        #[test]
        fn feedback_and_failed_enqueue_preserve_ownership() {
            let data = r#"{"command":"ok"}"#;
            let mut state = AttachedApprovals::default();
            state.apply(request("a", data), false);
            let (tx, mut rx) = mpsc::unbounded_channel();
            paint(&mut state);
            state.key(key(KeyCode::Char('f')), false, "", &tx);
            state.key(key(KeyCode::Char('x')), false, "", &tx);
            state.key(key(KeyCode::Enter), false, "", &tx);
            assert!(
                matches!(rx.try_recv().unwrap(), ClientFrame::ApprovalResponse {
                outcome: ApprovalOutcome::Reject { feedback: Some(feedback) }, ..
            } if feedback == "x")
            );
            state.apply(request("b", data), false);
            paint(&mut state);
            drop(rx);
            state.key(key(KeyCode::Char('y')), false, "", &tx);
            assert!(state.pending.contains_key(&RequestId("b".into())));
        }

        #[test]
        fn always_deny_escape_and_refusal_lifecycle() {
            use crate::adapters::daemon::protocol::ApprovalRefusal;
            use crate::domain::models::ApprovalScope;
            let data = r#"{"command":"ok"}"#;
            for (code, expected) in [
                (
                    KeyCode::Char('a'),
                    ApprovalOutcome::AlwaysAndSave {
                        scope: ApprovalScope::Tool("Bash".into()),
                    },
                ),
                (
                    KeyCode::Char('n'),
                    ApprovalOutcome::Reject { feedback: None },
                ),
                (KeyCode::Esc, ApprovalOutcome::Reject { feedback: None }),
            ] {
                let mut state = AttachedApprovals::default();
                state.apply(request("a", data), false);
                chunk(&mut state, "a", 0, data);
                complete(&mut state, "a", 1);
                let (tx, mut rx) = mpsc::unbounded_channel();
                paint(&mut state);
                state.key(key(KeyCode::Char('r')), false, "", &tx);
                paint(&mut state);
                state.key(key(KeyCode::Esc), false, "", &tx);
                assert!(state.review.is_none() && state.pending.len() == 1);
                assert!(rx.try_recv().is_err());
                paint(&mut state);
                state.key(key(code), false, "", &tx);
                assert!(
                    matches!(rx.try_recv().unwrap(), ClientFrame::ApprovalResponse {
                    request_id, outcome,
                } if request_id.0 == "a" && outcome == expected)
                );
            }
            let mut state = AttachedApprovals::default();
            state.apply(request("a", data), false);
            for reason in [
                ApprovalRefusal::WriterOnly,
                ApprovalRefusal::IssuedElsewhere,
            ] {
                assert_eq!(
                    state.apply(
                        DaemonFrame::ApprovalRefused {
                            request_id: RequestId("a".into()),
                            reason,
                        },
                        false
                    ),
                    Some(reason.to_string())
                );
                assert_eq!(state.pending.len(), 1);
            }
            state.apply(
                DaemonFrame::ApprovalRefused {
                    request_id: RequestId("a".into()),
                    reason: ApprovalRefusal::NoLongerPending,
                },
                false,
            );
            assert!(state.pending.is_empty() && state.order.is_empty());
        }

        #[test]
        fn retained_count_is_read_only_point_in_time_notice() {
            assert_eq!(
                retained_approval_attach_notice(2, true).as_deref(),
                Some("2 approval(s) waiting for the writer")
            );
            assert!(retained_approval_attach_notice(0, true).is_none());
            assert!(retained_approval_attach_notice(2, false).is_none());
        }

        #[test]
        fn read_only_never_admits_private_input() {
            let mut state = AttachedApprovals::default();
            let data = r#"{"command":"private"}"#;
            state.apply(request("a", data), true);
            state.apply(
                DaemonFrame::ApprovalInputChunk {
                    request_id: RequestId("a".into()),
                    sequence: 0,
                    data: data.into(),
                },
                true,
            );
            state.apply(
                DaemonFrame::ApprovalInputComplete {
                    request_id: RequestId("a".into()),
                    chunks: 1,
                },
                true,
            );
            assert!(state.pending.is_empty() && state.order.is_empty());
            assert!(paint(&mut state).trim().is_empty());
        }
    }

    /// In-crate nearest-production-path seams, not native socket/PTY evidence.
    mod attached_proposals {
        use super::*;
        use crate::adapters::daemon::protocol::{TeamProposalCloseOutcome, TeamSendOutcome};
        use crate::domain::models::{FeedbackLevel, SemanticMessageType, TeamProposalDecision};
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};

        struct Surface {
            state: TuiState,
            tab: TabRenderState,
            requests: AttachedTeamRequests,
            input: String,
            tx: mpsc::UnboundedSender<ClientFrame>,
            rx: mpsc::UnboundedReceiver<ClientFrame>,
            scroll_offset: usize,
            auto_scroll: bool,
            streaming: StreamingState,
        }

        impl Surface {
            fn new(body: &str) -> Self {
                let (tx, rx) = mpsc::unbounded_channel();
                let mut surface = Self {
                    state: TuiState::new(80, 24),
                    tab: TabRenderState::default(),
                    requests: AttachedTeamRequests::default(),
                    input: String::new(),
                    tx,
                    rx,
                    scroll_offset: 0,
                    auto_scroll: true,
                    streaming: StreamingState::default(),
                };
                surface.frame(DaemonFrame::TeamProposal {
                    proposal: "tp_attached".into(),
                    recipients: vec!["lena-po".into(), "jun-dev".into()],
                    message_type: SemanticMessageType::ArchitectureUpdate,
                    body: body.into(),
                });
                surface
            }

            fn frame(&mut self, frame: DaemonFrame) {
                apply_attached_proposal_frame(
                    &mut self.state,
                    &mut self.tab,
                    &mut self.requests,
                    "conversation",
                    frame,
                );
            }

            fn key(&mut self, code: KeyCode, higher: bool, read_only: bool) -> bool {
                let consumed = attached_proposal_key(
                    KeyEvent::new(code, KeyModifiers::NONE),
                    higher,
                    read_only,
                    &self.input,
                    &mut self.state,
                    &mut self.tab,
                    &mut self.requests,
                    &self.tx,
                    &mut self.scroll_offset,
                    &mut self.auto_scroll,
                );
                if !consumed && matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
                    scroll_attached_transcript(
                        code == KeyCode::PageUp,
                        self.state.viewport_height,
                        self.streaming.is_streaming,
                        &mut self.scroll_offset,
                        &mut self.auto_scroll,
                    );
                    return true;
                }
                consumed
            }

            fn paint(&mut self, width: u16, height: u16, higher: bool, read_only: bool) -> String {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut visible = None;
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        let display_input =
                            crate::adapters::tui::handlers::recipient_actions::pending_composition(
                                &self.state,
                            )
                            .unwrap_or(&self.input);
                        let mut layout = crate::adapters::tui::layout::compute_layout(
                            area,
                            &self.state.theme,
                            display_input,
                            1,
                            false,
                            DensityMode::Focus,
                        )
                        .unwrap();
                        let card = prepare_attached_proposal(
                            &self.state,
                            &self.tab,
                            &self.input,
                            higher,
                            read_only,
                            &mut layout,
                            area,
                        );
                        chat_pane::render_attached(
                            frame,
                            layout.chat_pane,
                            &Conversation::default(),
                            &self.streaming,
                            self.scroll_offset,
                            self.auto_scroll,
                            &self.state.theme,
                            &mut self.tab,
                            &HashMap::new(),
                            &self.state.feedback_blocks,
                            self.state.active_feedback_id.as_deref(),
                            &HashMap::new(),
                        );
                        crate::adapters::tui::widgets::input_box::render(
                            frame,
                            layout.input_area,
                            display_input,
                            display_input.chars().count(),
                            self.state.focus.clone(),
                            &self.state.theme,
                            false,
                            0,
                            None,
                        );
                        let actions = recipient_surface_actions(
                            &self.state,
                            &Conversation::default(),
                            read_only,
                        );
                        if let Some(surface) = self.state.recipient_surface.as_mut() {
                            render_recipient_surface(
                                frame,
                                layout.chat_pane,
                                surface,
                                &self.state.recipient_actions,
                                &actions,
                                // No conversation, so no pinned retro card.
                                None,
                                read_only,
                                &self.state.theme,
                            );
                        }
                        self.state.viewport_height = layout.chat_pane.height;
                        self.state.team_proposal_scroll_max =
                            card.as_ref().map_or(0, |card| card.scroll_max());
                        self.state.team_proposal_scroll = self
                            .state
                            .team_proposal_scroll
                            .min(self.state.team_proposal_scroll_max);
                        self.state.team_proposal_viewport_rows =
                            card.as_ref().map_or(0, |card| card.viewport_rows());
                        self.state.team_proposal_footer_visible =
                            card.as_ref().is_some_and(|card| card.footer_visible());
                        visible = crate::adapters::tui::widgets::team_proposal_card::render(
                            card,
                            frame.buffer_mut(),
                            &self.state.theme,
                        );
                    })
                    .unwrap();
                self.state.team_proposal_visible = visible;
                let buffer = terminal.backend().buffer();
                (0..height)
                    .map(|y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }

            fn assert_send(&mut self) {
                assert!(self.key(KeyCode::Char('y'), false, false));
                assert!(
                    matches!(
                        self.rx.try_recv().unwrap(),
                        ClientFrame::TeamProposalDecision { proposal, decision: TeamProposalDecision::Send }
                            if proposal == "tp_attached"
                    ),
                    "attached Send must emit exactly the snapshot proposal decision"
                );
                assert!(
                    self.rx.try_recv().is_err(),
                    "one human Send emits one frame"
                );
            }

            fn submit_pending_reply(&mut self, draft: &str, reply: &str) {
                self.input = draft.to_owned();
                assert!(
                    matches!(
                        attached_composer_key_with_team_requests(
                            KeyCode::Enter,
                            false,
                            &mut self.input,
                            &mut self.state,
                            &self.tx,
                            &mut self.requests,
                        ),
                        ComposerOutcome::FrameSent
                    ),
                    "the real composer must submit the recipient reply before testing proposal guards"
                );
                assert!(
                    matches!(
                        self.rx.try_recv().unwrap(),
                        ClientFrame::ReplyToRecipientItem { item_id, text, .. }
                            if item_id == "ri_pending" && text == reply
                    ),
                    "the recipient reply must retain its target and exact operator-authored body"
                );
                assert!(
                    self.input.is_empty(),
                    "begin_action must move the submitted draft out of the local buffer"
                );
                assert_eq!(
                    crate::adapters::tui::handlers::recipient_actions::pending_composition(
                        &self.state
                    ),
                    Some(draft),
                    "the real pending-action producer must retain the displayed draft"
                );
                assert!(
                    self.rx.try_recv().is_err(),
                    "submitting a recipient reply must not submit a proposal decision"
                );
                // Intentionally withhold the recipient result until resolve_pending_reply:
                // channel consumption, not a sleep, establishes the pending interval.
            }

            fn resolve_pending_reply(&mut self) {
                use crate::adapters::daemon::protocol::{
                    RecipientAction, RecipientActionOutcome, RecipientActionTarget,
                };
                crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                    &mut self.state,
                    &mut self.input,
                    RecipientAction::Reply,
                    RecipientActionTarget::Item {
                        item_id: "ri_pending".into(),
                    },
                    RecipientActionOutcome::Applied { reply: None },
                );
                assert!(
                    crate::adapters::tui::handlers::recipient_actions::pending_composition(
                        &self.state
                    )
                    .is_none(),
                    "the correlated recipient result must end the displayed pending composition"
                );
                assert!(
                    self.input.is_empty(),
                    "successful reply settlement must leave the composer genuinely empty"
                );
            }
        }

        #[test]
        fn attached_overflow_proposal_pages_before_authorizing_send() {
            let mut surface = Surface::new("body");
            surface.frame(DaemonFrame::TeamProposal {
                proposal: "tp_attached".into(),
                recipients: (0..12)
                    .map(|index| format!("member-{index:02}-dev"))
                    .collect(),
                message_type: SemanticMessageType::ArchitectureUpdate,
                body: "Exact attached overflow body.".into(),
            });
            let first = surface.paint(60, 16, false, false);
            assert!(first.contains("PgUp/PgDn"), "{first}");
            assert!(surface.key(KeyCode::Char('y'), false, false));
            assert!(
                surface.rx.try_recv().is_err(),
                "hidden footer must not authorize Send"
            );
            while surface.state.team_proposal_scroll < surface.state.team_proposal_scroll_max {
                assert!(surface.key(KeyCode::PageDown, false, false));
                if surface.state.team_proposal_scroll == surface.state.team_proposal_scroll_max {
                    assert!(surface.key(KeyCode::Char('y'), false, false));
                    assert!(
                        surface.rx.try_recv().is_err(),
                        "paging must paint the footer before Send"
                    );
                }
                surface.paint(60, 16, false, false);
            }
            let last = surface.paint(60, 16, false, false);
            assert!(last.contains("[n] Dismiss"), "{last}");
            surface.assert_send();
        }

        #[test]
        fn attached_overflow_review_paging_reaches_the_complete_body() {
            let live_text = (0..60)
                .map(|index| format!("LIVE_STREAM_{index}"))
                .collect::<Vec<_>>()
                .join("\n\n");
            for stream in [None, Some(String::new()), Some(live_text)] {
                let body = (0..30)
                    .map(|index| format!("REVIEW_ROW_{index:02}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut surface = Surface::new(&body);
                surface.streaming.is_streaming = stream.is_some();
                surface.streaming.current_text_buffer = stream.unwrap_or_default();
                surface.frame(DaemonFrame::TeamProposal {
                    proposal: "tp_attached".into(),
                    recipients: (0..12)
                        .map(|index| format!("member-{index:02}-dev"))
                        .collect(),
                    message_type: SemanticMessageType::ArchitectureUpdate,
                    body,
                });
                surface.paint(60, 16, false, false);
                assert!(surface.key(KeyCode::Char('r'), false, false));
                let mut pages = vec![surface.paint(60, 16, false, false)];
                for _ in 0..20 {
                    assert!(surface.key(KeyCode::PageUp, false, false));
                    pages.push(surface.paint(60, 16, false, false));
                }
                for index in 0..30 {
                    assert!(
                        pages
                            .iter()
                            .any(|page| page.contains(&format!("REVIEW_ROW_{index:02}"))),
                        "full Review row {index} is inaccessible: {pages:#?}"
                    );
                }
                assert!(
                    surface.rx.try_recv().is_err(),
                    "Review paging must never send"
                );
                assert!(surface.key(KeyCode::Char('n'), false, false));
                assert!(matches!(
                    surface.rx.try_recv().unwrap(),
                    ClientFrame::TeamProposalDecision {
                        decision: TeamProposalDecision::Dismiss,
                        ..
                    }
                ));
            }
        }

        #[test]
        fn attached_proposal_enter_and_escape_never_send() {
            let mut surface = Surface::new("body");
            surface.paint(80, 24, false, false);
            for key in [KeyCode::Enter, KeyCode::Esc] {
                assert!(surface.key(key, false, false));
                assert!(
                    surface.rx.try_recv().is_err(),
                    "attached Enter/Escape must not send a proposal"
                );
                assert!(attached_proposal_pending(&surface.state).is_some());
                assert!(surface.state.team_send_blocks.is_empty());
            }
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_nonempty_composer_owns_y_r_n_and_t9() {
            let mut surface = Surface::new("body");
            surface.input = "draft ".into();
            let text = surface.paint(80, 24, false, false);
            assert!(
                text.contains("Finish or clear your draft to decide."),
                "attached draft must paint T9"
            );
            assert!(!text.contains("[y] Send to"));
            for letter in ['y', 'r', 'n'] {
                assert!(
                    !surface.key(KeyCode::Char(letter), false, false),
                    "composer letters must not decide an attached proposal"
                );
                attached_composer_key_with_team_requests(
                    KeyCode::Char(letter),
                    false,
                    &mut surface.input,
                    &mut surface.state,
                    &surface.tx,
                    &mut surface.requests,
                );
            }
            assert_eq!(
                surface.input, "draft yrn",
                "attached proposal keys must preserve and edit the draft"
            );
            assert!(
                surface.rx.try_recv().is_err(),
                "composer letters must not decide an attached proposal"
            );
            assert!(surface.tab.team_proposal_reviews.is_empty());
            surface.input.clear();
            surface.paint(80, 24, false, false);
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_pending_reply_composition_cannot_authorize_send() {
            use crate::adapters::tui::handlers::recipient_actions::pending_composition;
            let reply = "operator draft e\u{301}界";
            let draft = format!("/team reply ri_pending {reply}");
            let mut surface = Surface::new("proposal body distinct from the operator reply");
            let snapshot = attached_proposal_pending(&surface.state).unwrap().clone();
            surface.submit_pending_reply(&draft, reply);
            let painted = surface.paint(80, 24, false, false);
            assert!(
                painted.contains(&draft),
                "the real input renderer must still show the submitted draft while its result is delayed"
            );
            assert!(
                Arc::ptr_eq(
                    surface.state.team_proposal_visible.as_ref().unwrap(),
                    &snapshot
                ),
                "the pending-input guard must be exercised against an actually painted proposal"
            );

            let handled = surface.key(KeyCode::Char('y'), false, false);
            assert!(
                surface.rx.try_recv().is_err(),
                "pending recipient composition must not authorize a proposal Send merely because the local buffer was taken"
            );
            assert!(
                !handled,
                "a visible nonempty pending draft must retain ownership of proposal decision letters"
            );
            assert!(
                Arc::ptr_eq(
                    attached_proposal_pending(&surface.state).unwrap(),
                    &snapshot
                ),
                "blocked Send must preserve the exact immutable waiting proposal"
            );
            assert_eq!(
                pending_composition(&surface.state),
                Some(draft.as_str()),
                "blocked proposal Send must preserve the pending recipient composition byte-for-byte"
            );
            assert!(surface.requests.pending.is_empty());
            assert!(surface.state.team_send_blocks.is_empty());

            surface.resolve_pending_reply();
            surface.paint(80, 24, false, false);
            assert!(Arc::ptr_eq(
                attached_proposal_pending(&surface.state).unwrap(),
                &snapshot
            ));
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_pending_reply_card_uses_existing_draft_disabled_state() {
            use crate::adapters::tui::handlers::recipient_actions::pending_composition;
            let reply = "operator draft e\u{301}界";
            let draft = format!("/team reply ri_pending {reply}");
            let mut surface = Surface::new("proposal body distinct from the operator reply");
            let snapshot = attached_proposal_pending(&surface.state).unwrap().clone();
            surface.input = draft.clone();
            let local_draft = surface.paint(80, 24, false, false);
            let card_rows = |text: &str| {
                text.lines()
                    .filter(|row| row.contains('║') || row.contains('╔') || row.contains('╚'))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            };
            let disabled_card = card_rows(&local_draft);
            assert!(
                !disabled_card.is_empty(),
                "the local-draft control must paint a proposal card"
            );
            assert!(
                !disabled_card.iter().any(|row| row.contains("[y]")),
                "the existing nonempty-composer control must disable the proposal Send action"
            );

            surface.submit_pending_reply(&draft, reply);
            let pending_draft = surface.paint(80, 24, false, false);
            assert!(
                pending_draft.contains(&draft),
                "the pending recipient draft must remain visible in the real composer renderer"
            );
            assert_eq!(
                card_rows(&pending_draft),
                disabled_card,
                "pending recipient composition must paint the same T9/action-disabled card as the identical local draft, not offer Send"
            );
            assert!(Arc::ptr_eq(
                attached_proposal_pending(&surface.state).unwrap(),
                &snapshot
            ));
            assert_eq!(pending_composition(&surface.state), Some(draft.as_str()));
            assert!(surface.rx.try_recv().is_err());

            surface.resolve_pending_reply();
            let empty_composer = surface.paint(80, 24, false, false);
            assert!(
                card_rows(&empty_composer)
                    .iter()
                    .any(|row| row.contains("[y]")),
                "after the recipient result clears the displayed composer, the waiting proposal must offer Send again"
            );
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_dismiss_is_only_a_decision_and_shared_info() {
            let mut surface = Surface::new("private body");
            surface.paint(80, 24, false, false);
            assert!(surface.key(KeyCode::Char('n'), false, false));
            assert!(
                matches!(
                    surface.rx.try_recv().unwrap(),
                    ClientFrame::TeamProposalDecision {
                        decision: TeamProposalDecision::Dismiss,
                        ..
                    }
                ),
                "attached Dismiss must not emit a Send frame"
            );
            assert!(surface.rx.try_recv().is_err());
            assert!(surface.requests.pending.is_empty());
            assert!(
                surface.state.team_send_blocks.is_empty(),
                "attached Dismiss must not open a send block"
            );
            assert!(attached_proposal_pending(&surface.state).is_none());
            let block = &surface.state.feedback_blocks["team-proposal-status"];
            assert_eq!(block.level, FeedbackLevel::Info);
            assert_eq!(block.message, "Proposal dismissed. Nothing was sent.");
            surface.frame(DaemonFrame::TeamProposalClosed {
                proposal: "tp_attached".into(),
                outcome: TeamProposalCloseOutcome::Dismissed,
            });
            assert!(
                surface
                    .paint(80, 24, false, false)
                    .contains("Proposal dismissed. Nothing was sent.")
            );
            let mut positive = Surface::new("private body");
            positive.paint(80, 24, false, false);
            positive.assert_send();
        }

        #[test]
        fn attached_review_repositions_transcript_after_history_scrolling() {
            let old_body = (0..150)
                .map(|line| format!("OLDER_REVIEW_{line}"))
                .collect::<Vec<_>>()
                .join("\n");
            let mut surface = Surface::new(&old_body);
            surface.paint(80, 24, false, false);
            assert!(surface.key(KeyCode::Char('r'), false, false));
            assert!(surface.key(KeyCode::Char('n'), false, false));
            assert!(matches!(
                surface.rx.try_recv().unwrap(),
                ClientFrame::TeamProposalDecision {
                    decision: TeamProposalDecision::Dismiss,
                    ..
                }
            ));
            surface.frame(DaemonFrame::TeamProposal {
                proposal: "tp_current".into(),
                recipients: vec!["lena-po".into()],
                message_type: SemanticMessageType::ArchitectureUpdate,
                body: "CURRENT_REVIEW_FIRST\nCURRENT_REVIEW_LAST".into(),
            });
            surface.scroll_offset = 100;
            surface.auto_scroll = false;
            let before = surface.paint(80, 24, false, false);
            assert!(before.contains("OLDER_REVIEW_"), "{before}");
            assert!(surface.key(KeyCode::Char('r'), false, false));
            let reviewed = surface.paint(80, 24, false, false);
            for line in ["CURRENT_REVIEW_FIRST", "CURRENT_REVIEW_LAST"] {
                assert!(reviewed.contains(&format!("│ {line}")), "{reviewed}");
            }
            assert!(surface.rx.try_recv().is_err());
            assert_eq!(
                attached_proposal_pending(&surface.state).unwrap().id,
                "tp_current"
            );

            assert!(surface.key(KeyCode::PageUp, false, false));
            let earlier = surface.paint(80, 24, false, false);
            assert!(earlier.contains("OLDER_REVIEW_"), "{earlier}");
            assert!(surface.key(KeyCode::Char('r'), false, false));
            let reopened = surface.paint(80, 24, false, false);
            assert!(reopened.contains("│ CURRENT_REVIEW_LAST"), "{reopened}");
            assert!(surface.rx.try_recv().is_err());
        }

        #[tokio::test]
        async fn attached_review_replaces_an_open_received_item_surface() {
            use crate::adapters::tui::state::RecipientSurfaceAccess;

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
            let mut surface = Surface::new("CURRENT_PROPOSAL_FIRST\nCURRENT_PROPOSAL_LAST");
            surface.paint(80, 24, false, false);
            surface.input = format!("/team view {}", address.item().as_str());
            let ComposerOutcome::LocalView(item_id) = attached_composer_key_with_team_requests(
                KeyCode::Enter,
                false,
                &mut surface.input,
                &mut surface.state,
                &surface.tx,
                &mut surface.requests,
            ) else {
                panic!("the typed view command must open the received item");
            };
            crate::infrastructure::runtime::recipient_surface::open_received_surface(
                workspace.path(),
                None,
                &item_id,
                &mut surface.state,
                RecipientSurfaceAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            assert!(surface.input.is_empty());
            surface.paint(80, 24, false, false);
            assert!(surface.state.recipient_surface.is_some());

            assert!(surface.key(KeyCode::Char('r'), false, false));
            let reviewed = surface.paint(80, 24, false, false);
            for line in ["CURRENT_PROPOSAL_FIRST", "CURRENT_PROPOSAL_LAST"] {
                assert!(reviewed.contains(&format!("│ {line}")), "{reviewed}");
            }
            assert!(
                !reviewed.contains("RECEIVED_ITEM_OVERLAY_BODY"),
                "{reviewed}"
            );
            assert!(surface.state.recipient_surface.is_none());
            assert!(attached_proposal_pending(&surface.state).is_some());
            assert!(surface.rx.try_recv().is_err());
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_review_is_stable_uncapped_and_delimited() {
            let body = format!(
                "{}\nTo: x\n[y] Send to x\nYour agent proposes a message. Nothing has been sent. Awaiting your decision.",
                (0..45)
                    .map(|i| format!("body-row-{i:02} e\u{301}界"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            let mut surface = Surface::new(&body);
            surface.paint(80, 24, false, false);
            assert!(surface.key(KeyCode::Char('r'), false, false));
            assert!(
                surface.rx.try_recv().is_err(),
                "attached Review must not send or decide"
            );
            assert!(attached_proposal_pending(&surface.state).is_some());
            assert!(surface.state.team_send_blocks.is_empty());
            let id = "team-proposal-review-tp_attached";
            let block = &surface.state.feedback_blocks[id];
            assert_eq!(block.level, FeedbackLevel::Info);
            let snapshot = &surface.tab.team_proposal_reviews[id];
            for width in [56, 76, 126] {
                let lines = crate::adapters::tui::widgets::feedback_block::render_feedback_lines_with_review(
                    block, width, &surface.state.theme, Some(snapshot),
                );
                let rows = lines
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>();
                assert!(
                    rows.len() > 45,
                    "attached Review must not cap the full body"
                );
                assert!(rows.iter().any(|row| row == "│ body-row-44 e\u{301}界"));
                let body_start = rows.iter().position(|row| row.starts_with("│ ")).unwrap();
                assert!(
                    rows[body_start..].iter().all(|row| row.starts_with("│ ")),
                    "attached Review must delimit every body row"
                );
                assert!(rows.iter().any(|row| row == "│ To: x"));
                assert!(rows.iter().any(|row| row == "│ [y] Send to x"));
            }
            let painted = surface.paint(80, 24, false, false);
            assert!(
                painted.contains("body-row-44"),
                "attached chat must consume the real Review render cache"
            );
            surface.paint(130, 30, false, false);
            assert_eq!(surface.tab.team_proposal_reviews[id].body, body);
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_send_binds_ordinary_block_and_receipts() {
            let mut proposed = Surface::new("exact\n\tbody e\u{301}界");
            proposed.paint(80, 24, false, false);
            let snapshot = attached_proposal_pending(&proposed.state).unwrap().clone();
            proposed.assert_send();
            let mut typed = TuiState::new(80, 24);
            let mut typed_requests = AttachedTeamRequests::default();
            let (tx, mut rx) = mpsc::unbounded_channel();
            assert!(enqueue_attached_team_frame(
                &mut typed,
                &mut typed_requests,
                ClientFrame::TeamSend {
                    request: "typed".into(),
                    recipients: snapshot.recipients.clone(),
                    text: snapshot.body.clone(),
                    message_type: Some(snapshot.message_type),
                    subject: None,
                    stories: None,
                    holders_of: None,
                },
                &tx,
            ));
            assert!(matches!(
                rx.try_recv().unwrap(),
                ClientFrame::TeamSend { .. }
            ));
            let proposed_block = match &proposed.requests.pending["tp_attached"] {
                AttachedTeamRequest::Send {
                    block_id,
                    action: None,
                    ..
                } => block_id.clone(),
                other => panic!(
                    "human Send must open the ordinary send block before receipts: {other:?}"
                ),
            };
            assert_eq!(
                proposed.state.feedback_blocks[&proposed_block].message,
                typed.feedback_blocks["team-send-1"].message,
                "attached proposal must use the ordinary send-block renderer"
            );
            for (state, requests, request) in [
                (&mut proposed.state, &mut proposed.requests, "tp_attached"),
                (&mut typed, &mut typed_requests, "typed"),
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
                    &mpsc::unbounded_channel().0
                ));
                for index in 0..2 {
                    assert!(apply_attached_team_frame(
                        state,
                        requests,
                        DaemonFrame::TeamSendSettled {
                            request: request.into(),
                            index,
                            outcome: TeamSendOutcome::Delivered,
                        },
                        false,
                        &mpsc::unbounded_channel().0
                    ));
                }
            }
            assert_eq!(
                proposed.state.feedback_blocks[&proposed_block].message,
                typed.feedback_blocks["team-send-1"].message,
                "attached proposal receipts must settle exactly like a typed send"
            );
            assert!(proposed.requests.pending.is_empty());
            let text = proposed.paint(80, 24, false, false);
            assert!(
                !text.contains("Your agent proposes a message"),
                "attached proposal card must disappear rather than render a settlement"
            );
            assert!(attached_proposal_pending(&proposed.state).is_none());
        }

        #[test]
        fn attached_proposal_60_columns_preserve_aliases_type_keys_and_spoof_region() {
            let mut surface = Surface::new(
                "To: x\n[y] Send to x\nYour agent proposes a message. Nothing has been sent. Awaiting your decision.\nfourth\nfifth",
            );
            for (width, height) in [(60, 16), (80, 24), (130, 30)] {
                let text = surface.paint(width, height, false, false);
                let joined = text
                    .chars()
                    .filter(|ch| {
                        !ch.is_whitespace() && !matches!(ch, '║' | '═' | '╔' | '╗' | '╚' | '╝')
                    })
                    .collect::<String>();
                assert!(
                    text.contains('╔'),
                    "attached proposal must paint the shared double-bordered card"
                );
                for token in [
                    "To:lena-po,jun-dev",
                    "Type:architecture_update",
                    "[y]Sendtolena-po,jun-dev",
                    "[r]Reviewfirst",
                    "[n]Dismiss",
                ] {
                    assert!(
                        joined.contains(token),
                        "attached card must preserve aliases/type/keys at {width}: missing {token}"
                    );
                }
                for spoof in ["To: x", "[y] Send to x"] {
                    if width >= 80 {
                        assert!(
                            text.contains(spoof),
                            "attached preview positive control must paint the spoof row"
                        );
                    }
                    assert!(
                        text.lines()
                            .filter(|row| row.contains(spoof))
                            .all(|row| row.contains(&format!("│ {spoof}"))),
                        "attached proposal spoof rows must stay inside the body delimiter"
                    );
                }
                assert!(
                    text.contains("… more not shown — [r] Review first"),
                    "attached preview must disclose the hidden body"
                );
                assert!(
                    !text.contains("Delivered"),
                    "attached card must never paint a send outcome"
                );
            }
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_higher_confirmations_and_raw_key_fences_keep_ownership() {
            let mut surface = Surface::new("body");
            for (consent, consolidation, retract) in [
                (true, false, false),
                (false, true, false),
                (false, false, true),
            ] {
                let higher = attached_proposal_higher_confirmation(
                    &surface.state,
                    consent,
                    consolidation,
                    retract,
                );
                assert!(higher);
                surface.paint(80, 24, false, false);
                assert!(
                    !surface.key(KeyCode::Char('y'), higher, false),
                    "attached higher confirmation must retain its Send key"
                );
                assert!(
                    !surface
                        .paint(80, 24, higher, false)
                        .contains("Your agent proposes a message")
                );
            }
            surface.state.pending_permission =
                Some(crate::adapters::tui::state::PendingPermission {
                    id: crate::domain::models::RequestId::new(),
                    source: crate::domain::models::ApprovalSource::ForegroundTurn {
                        conversation_id: surface
                            .state
                            .team_proposal_conversation_id
                            .clone()
                            .unwrap(),
                    },
                    tool_name: "Write".into(),
                    input: std::sync::Arc::new(serde_json::json!({})),
                    risk: crate::domain::models::ToolRisk::Safe,
                });
            assert!(
                attached_proposal_higher_confirmation(&surface.state, false, false, false),
                "attached permission prompts must outrank proposals"
            );
            assert!(!surface.key(KeyCode::Char('y'), true, false));
            surface.state.pending_permission = None;
            surface.state.team_proposal_visible = None;
            assert!(
                !surface.key(KeyCode::Char('y'), false, false),
                "unpainted attached proposal cannot decide"
            );
            surface.paint(80, 24, false, false);
            for key in [
                KeyEvent::new_with_kind(
                    KeyCode::Char('y'),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                ),
                KeyEvent::new_with_kind(
                    KeyCode::Char('n'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT),
            ] {
                assert!(attached_proposal_key(
                    key,
                    false,
                    false,
                    "",
                    &mut surface.state,
                    &mut surface.tab,
                    &mut surface.requests,
                    &surface.tx,
                    &mut surface.scroll_offset,
                    &mut surface.auto_scroll,
                ));
            }
            assert!(
                surface.rx.try_recv().is_err(),
                "attached repeat/release/modified keys must not decide"
            );
            assert!(surface.tab.team_proposal_reviews.is_empty());
            assert!(!surface.key(KeyCode::Char('y'), false, true));
            assert!(
                !surface
                    .paint(80, 24, false, true)
                    .contains("Your agent proposes a message")
            );
            surface.paint(80, 24, false, false);
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_refusal_withdrawal_and_reemit_are_correlated() {
            let mut surface = Surface::new("body");
            surface.paint(80, 24, false, false);
            surface.assert_send();
            let busy = "Not sent yet — this daemon is busy sending. The proposal is still waiting.";
            surface.frame(DaemonFrame::TeamProposalClosed {
                proposal: "tp_attached".into(),
                outcome: TeamProposalCloseOutcome::Refused {
                    reason: busy.into(),
                },
            });
            assert!(surface.requests.pending.is_empty());
            assert!(surface.state.team_send_blocks.is_empty());
            assert_eq!(
                surface.state.feedback_blocks["team-proposal-status"].message,
                busy
            );
            assert!(
                attached_proposal_pending(&surface.state).is_none(),
                "a refusal alone must not resurrect a terminal attached proposal"
            );
            surface.frame(DaemonFrame::TeamProposal {
                proposal: "tp_attached".into(),
                recipients: vec!["lena-po".into(), "jun-dev".into()],
                message_type: SemanticMessageType::ArchitectureUpdate,
                body: "body".into(),
            });
            surface.paint(80, 24, false, false);
            assert!(surface.key(KeyCode::Char('r'), false, false));
            surface.frame(DaemonFrame::TeamProposalClosed {
                proposal: "unrelated".into(),
                outcome: TeamProposalCloseOutcome::Withdrawn,
            });
            assert!(
                attached_proposal_pending(&surface.state).is_some(),
                "an unrelated close must not remove the current attached snapshot"
            );
            surface.frame(DaemonFrame::TeamProposalClosed {
                proposal: "tp_attached".into(),
                outcome: TeamProposalCloseOutcome::Withdrawn,
            });
            assert!(attached_proposal_pending(&surface.state).is_none());
            assert!(surface.tab.team_proposal_reviews.is_empty());
            assert_eq!(
                surface.state.feedback_blocks["team-proposal-status"].message,
                "Proposal withdrawn with the conversation. Nothing was sent."
            );
            assert!(
                surface.rx.try_recv().is_err(),
                "attached withdrawal must never submit a decision"
            );
            surface.state.team_proposal_conversation_id = Some("other-conversation".into());
            surface.frame(DaemonFrame::TeamProposal {
                proposal: "tp_attached".into(),
                recipients: vec!["lena-po".into(), "jun-dev".into()],
                message_type: SemanticMessageType::ArchitectureUpdate,
                body: "body".into(),
            });
            surface.paint(80, 24, false, false);
            surface.assert_send();
        }

        #[test]
        fn attached_proposal_conversation_binding_and_enqueue_failure_keep_snapshot() {
            let mut surface = Surface::new("exact body");
            surface.paint(80, 24, false, false);
            let snapshot = attached_proposal_pending(&surface.state).unwrap().clone();
            surface.state.team_proposal_conversation_id = Some("different-view".into());
            assert!(
                !surface.key(KeyCode::Char('y'), false, false),
                "attached proposal must not decide from a different conversation view"
            );
            assert!(
                !surface
                    .paint(80, 24, false, false)
                    .contains("Your agent proposes a message")
            );
            surface.state.team_proposal_conversation_id = Some("conversation".into());
            surface.paint(80, 24, false, false);
            assert!(Arc::ptr_eq(
                attached_proposal_pending(&surface.state).unwrap(),
                &snapshot
            ));
            surface.rx.close();
            assert!(surface.key(KeyCode::Char('y'), false, false));
            assert!(
                Arc::ptr_eq(
                    attached_proposal_pending(&surface.state).unwrap(),
                    &snapshot
                ),
                "failed attached decision enqueue must preserve the exact pending snapshot"
            );
            assert!(surface.requests.pending.is_empty());
            assert!(surface.state.team_send_blocks.is_empty());
            assert_eq!(
                surface.state.feedback_blocks["team-proposal-status"].message,
                "Attach socket closed — the proposal decision was not sent."
            );
            let mut positive = Surface::new("exact body");
            positive.paint(80, 24, false, false);
            positive.assert_send();
        }
    }

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

    #[tokio::test]
    async fn story19_23_design_update_acknowledgement_changes_only_slot_one_while_received() {
        use crate::adapters::tui::state::RecipientSurfaceAccess;
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
        super::super::recipient_surface::open_received_surface(
            workspace.path(),
            Some(&conversation),
            address.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
            super::super::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert_eq!(actions[1], Some(RecipientSurfaceAction::Acknowledge));
        state.recipient_surface.as_mut().unwrap().message_type = SemanticMessageType::Unknown;
        let baseline = recipient_surface_actions(&state, &conversation, false);
        for index in [0, 2, 3, 4, 5] {
            assert_eq!(actions[index], baseline[index]);
        }
        state.recipient_surface.as_mut().unwrap().message_type = SemanticMessageType::DesignUpdate;
        for item_state in ["acknowledged", "removed"] {
            state.recipient_surface.as_mut().unwrap().item_state = Some(item_state);
            assert_eq!(
                recipient_surface_actions(&state, &conversation, false)[1],
                None
            );
        }
    }

    #[tokio::test]
    async fn story19_24_status_surface_slots_and_access_boundaries() {
        use crate::adapters::tui::state::{RecipientSurfaceAccess, RecipientSurfaceTarget};
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
        super::super::recipient_surface::open_received_surface(
            workspace.path(),
            Some(&conversation),
            address.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
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
                    state.recipient_surface.as_mut().unwrap().message_type = message_type;
                    let control = conversation
                        .peer_row_controls
                        .get_mut("status-row")
                        .unwrap();
                    control.message_type = message_type;
                    control.stage = stage;
                    control.node_resolvable = resolvable;
                    control.status = None;
                    let baseline = recipient_surface_actions(&state, &conversation, false);
                    conversation
                        .peer_row_controls
                        .get_mut("status-row")
                        .unwrap()
                        .status = Some(StatusPreview::new(StatusDisclosure::None, &[]));
                    let actions = recipient_surface_actions(&state, &conversation, false);
                    // A restored (unresolvable) card keeps the status acts,
                    // as it keeps `[r] Respond`; the daemon's orphan arm answers.
                    if message_type == SemanticMessageType::StatusRequest
                        && stage == PeerRowStage::AwaitingOperator
                    {
                        assert!(
                            actions
                                == [
                                    Some(RecipientSurfaceAction::Respond),
                                    Some(RecipientSurfaceAction::Busy),
                                    Some(RecipientSurfaceAction::ShareStatus),
                                    None,
                                    Some(RecipientSurfaceAction::DeclineStatus),
                                    baseline[5],
                                ]
                        );
                    } else {
                        assert!(actions == baseline);
                    }
                }
            }
        }
        state.recipient_surface.as_mut().unwrap().message_type = SemanticMessageType::StatusRequest;
        let control = conversation
            .peer_row_controls
            .get_mut("status-row")
            .unwrap();
        control.stage = PeerRowStage::AwaitingOperator;
        control.node_resolvable = true;
        for access in [
            RecipientSurfaceAccess::ReadOnly,
            RecipientSurfaceAccess::LocalOnly,
        ] {
            state.recipient_surface.as_mut().unwrap().access = access;
            assert!(
                recipient_surface_actions(&state, &conversation, false)
                    .iter()
                    .all(Option::is_none)
            );
        }
        state.recipient_surface.as_mut().unwrap().access = RecipientSurfaceAccess::Writer;
        assert!(
            recipient_surface_actions(&state, &conversation, true)
                .iter()
                .all(Option::is_none)
        );
        state.recipient_surface.as_mut().unwrap().item_state = Some("removed");
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert!(!actions.contains(&Some(RecipientSurfaceAction::ShareStatus)));
        assert!(!actions.contains(&Some(RecipientSurfaceAction::DeclineStatus)));
        state.recipient_surface.as_mut().unwrap().item_state = Some("received");
        state.recipient_surface.as_mut().unwrap().target =
            RecipientSurfaceTarget::Node("status-node".to_owned());
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert!(!actions.contains(&Some(RecipientSurfaceAction::ShareStatus)));
        assert!(!actions.contains(&Some(RecipientSurfaceAction::DeclineStatus)));
    }

    #[tokio::test]
    async fn story19_24_status_keys_correlate_suppress_duplicates_and_release_failed_enqueue() {
        use crate::adapters::daemon::protocol::{RecipientAction, RecipientActionTarget};
        use crate::adapters::tui::state::{RecipientActionState, RecipientSurfaceAccess};
        use crate::domain::models::{
            PeerRowStage, SemanticMessageType, StatusDisclosure, StatusPreview,
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
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
        let conversation = Conversation {
            messages: vec![peer_row("status-row", "waiting")],
            peer_row_controls: [("status-row".to_owned(), control)].into_iter().collect(),
            ..Default::default()
        };
        for (key, decline) in [('s', false), ('n', true)] {
            let mut state = TuiState::new(80, 24);
            super::super::recipient_surface::open_received_surface(
                workspace.path(),
                Some(&conversation),
                address.item().as_str(),
                &mut state,
                RecipientSurfaceAccess::Writer,
                super::super::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            let mut input = "unfinished operator text".to_owned();
            let (tx, mut rx) = mpsc::unbounded_channel();
            recipient_surface_key(
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                workspace.path(),
                &conversation,
                &mut input,
                &mut state,
                Some(&tx),
            )
            .await;
            assert!(matches!(rx.try_recv().unwrap(),
                ClientFrame::ShareRecipientStatus { item_id, decline: actual }
                    if item_id == "ri_status_keys" && actual == decline));
            let action_key = (
                RecipientAction::ShareStatus,
                RecipientActionTarget::Item {
                    item_id: "ri_status_keys".to_owned(),
                },
            );
            assert!(matches!(
                state.recipient_actions.get(&action_key),
                Some(RecipientActionState::Pending { .. })
            ));
            let surface = state.recipient_surface.as_ref().unwrap();
            let notices =
                crate::adapters::tui::handlers::recipient_actions::surface_action_notices(
                    &state.recipient_actions,
                    &surface.target,
                    surface.node_id.as_deref(),
                )
                .map(|notice| notice.into_owned())
                .collect::<Vec<_>>();
            assert_eq!(notices, vec!["applying status reply for ri_status_keys…"]);
            let actions = recipient_surface_actions(&state, &conversation, false);
            assert!(actions[2].is_none() && actions[4].is_none());
            for duplicate in ['s', 'n'] {
                recipient_surface_key(
                    KeyEvent::new(KeyCode::Char(duplicate), KeyModifiers::NONE),
                    workspace.path(),
                    &conversation,
                    &mut input,
                    &mut state,
                    Some(&tx),
                )
                .await;
            }
            assert!(rx.try_recv().is_err());
            assert_eq!(input, "unfinished operator text");
            state.recipient_actions.clear();
            drop(rx);
            assert!(!queue_recipient_action(
                &mut state,
                &mut input,
                ClientFrame::ShareRecipientStatus {
                    item_id: "ri_status_keys".to_owned(),
                    decline
                },
                &tx,
                false,
            ));
            assert!(matches!(state.recipient_actions.get(&action_key),
                Some(RecipientActionState::Completed { notice }) if !notice.is_empty()));
            let actions = recipient_surface_actions(&state, &conversation, false);
            assert!(actions[2] == Some(RecipientSurfaceAction::ShareStatus));
            assert!(actions[4] == Some(RecipientSurfaceAction::DeclineStatus));
            assert_eq!(input, "unfinished operator text");
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

    /// Story 19.25 fixture: a journal-backed `retro_request` item, its pinned
    /// AwaitingOperator card with a stored preview, and a Writer surface.
    async fn open_retro_surface(item_id: &str) -> (tempfile::TempDir, Conversation, TuiState) {
        use crate::adapters::tui::state::RecipientSurfaceAccess;
        use crate::domain::models::{PeerRowStage, SemanticMessageType};
        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
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
        let mut state = TuiState::new(100, 40);
        super::super::recipient_surface::open_received_surface(
            workspace.path(),
            Some(&conversation),
            address.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
            super::super::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        (workspace, conversation, state)
    }

    async fn press_surface_key(
        key: char,
        workspace: &Path,
        conversation: &Conversation,
        input: &mut String,
        state: &mut TuiState,
        tx: &mpsc::UnboundedSender<ClientFrame>,
    ) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        recipient_surface_key(
            KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
            workspace,
            conversation,
            input,
            state,
            Some(tx),
        )
        .await;
    }

    #[tokio::test]
    async fn story19_25_retro_surface_slots_per_stage_type_and_restored_card() {
        use crate::adapters::tui::state::{RecipientSurfaceAccess, RecipientSurfaceTarget};
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
                    state.recipient_surface.as_mut().unwrap().message_type = message_type;
                    let control = conversation.peer_row_controls.get_mut("retro-row").unwrap();
                    control.message_type = message_type;
                    control.stage = stage;
                    control.node_resolvable = resolvable;
                    control.retro = None;
                    let baseline = recipient_surface_actions(&state, &conversation, false);
                    conversation
                        .peer_row_controls
                        .get_mut("retro-row")
                        .unwrap()
                        .retro = Some(retro_test_preview());
                    let actions = recipient_surface_actions(&state, &conversation, false);
                    // A restored (unresolvable) card keeps the retro acts,
                    // as it keeps `[r] Respond` (19-24 BH-1).
                    if message_type == SemanticMessageType::RetroRequest
                        && stage == PeerRowStage::AwaitingOperator
                    {
                        assert_eq!(
                            actions,
                            [
                                Some(RecipientSurfaceAction::Respond),
                                Some(RecipientSurfaceAction::Busy),
                                Some(RecipientSurfaceAction::RetroShare),
                                Some(RecipientSurfaceAction::RetroLeaveOut),
                                Some(RecipientSurfaceAction::RetroDecline),
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
        state.recipient_surface.as_mut().unwrap().message_type = SemanticMessageType::RetroRequest;
        let control = conversation.peer_row_controls.get_mut("retro-row").unwrap();
        control.message_type = SemanticMessageType::RetroRequest;
        control.stage = PeerRowStage::AwaitingOperator;
        control.node_resolvable = true;
        let no_overlay = std::collections::HashMap::new();
        assert!(
            retro_overlay(
                &conversation,
                state.recipient_surface.as_ref().unwrap(),
                &no_overlay
            )
            .is_some_and(|overlay| overlay.left_out.is_none())
        );
        for access in [
            RecipientSurfaceAccess::ReadOnly,
            RecipientSurfaceAccess::LocalOnly,
        ] {
            state.recipient_surface.as_mut().unwrap().access = access;
            assert!(
                recipient_surface_actions(&state, &conversation, false)
                    .iter()
                    .all(Option::is_none)
            );
        }
        state.recipient_surface.as_mut().unwrap().access = RecipientSurfaceAccess::Writer;
        assert!(
            recipient_surface_actions(&state, &conversation, true)
                .iter()
                .all(Option::is_none)
        );
        let retro_acts = [
            Some(RecipientSurfaceAction::RetroShare),
            Some(RecipientSurfaceAction::RetroLeaveOut),
            Some(RecipientSurfaceAction::RetroDecline),
        ];
        state.recipient_surface.as_mut().unwrap().item_state = Some("removed");
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert!(retro_acts.iter().all(|act| !actions.contains(act)));
        assert!(
            retro_overlay(
                &conversation,
                state.recipient_surface.as_ref().unwrap(),
                &no_overlay
            )
            .is_none()
        );
        state.recipient_surface.as_mut().unwrap().item_state = Some("received");
        state.recipient_surface.as_mut().unwrap().target =
            RecipientSurfaceTarget::Node("retro-node".to_owned());
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert!(retro_acts.iter().all(|act| !actions.contains(act)));
        assert!(
            retro_overlay(
                &conversation,
                state.recipient_surface.as_ref().unwrap(),
                &no_overlay
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn story19_25_retro_keys_toggle_overlay_hint_card_and_send_frames() {
        use crate::adapters::daemon::protocol::{
            ProtocolError, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
        };
        use crate::adapters::tui::handlers::recipient_actions::{
            apply_action_result, prune_retro_left_out, surface_action_notices,
        };
        use crate::adapters::tui::state::RecipientActionState;
        use crate::domain::models::{PeerRowStage, RetroField, RetroFieldSet};
        let item = "ri_retro_keys";
        let (workspace, mut conversation, mut state) = open_retro_surface(item).await;
        let workspace = workspace.path();
        let mut input = "unfinished operator text".to_owned();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let card = |conversation: &Conversation, state: &TuiState| {
            peer_response_card_lines(
                &pending_peer_card(conversation, &state.retro_left_out).unwrap(),
                false,
                60,
            )
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
        };
        let overlay = |state: &TuiState| state.retro_left_out.get(item).cloned();
        let set = |fields: &[RetroField]| fields.iter().copied().collect::<RetroFieldSet>();

        assert!(card(&conversation, &state).contains("  [2] reply times: 3 replies, median 25m"));
        let painted = paint_surface(&mut state, &conversation, 100, 40);
        assert!(painted.contains("Shares (last 14 days):"), "{painted}");
        assert!(painted.contains("[s] Share") && !painted.contains("with redactions"));
        assert!(painted.contains("[1-3] Leave out") && painted.contains("[n] Decline"));

        // `[2]` leaves reply times out: no frame, the card and the surface
        // (same width: the overlay bits invalidate its cache) both re-render.
        press_surface_key('2', workspace, &conversation, &mut input, &mut state, &tx).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(overlay(&state), Some(set(&[RetroField::ReplyTimes])));
        let text = card(&conversation, &state);
        assert!(text.contains("  [2] reply times: left out"), "{text}");
        assert!(
            text.contains("  Not shared: reply times · message content"),
            "{text}"
        );
        let painted = paint_surface(&mut state, &conversation, 100, 40);
        assert!(painted.contains("[2] reply times: left out"), "{painted}");
        assert!(painted.contains("Not shared: reply times · message content"));
        assert!(painted.contains("[s] Share with redactions"), "{painted}");

        // Toggling back empties (and drops) the overlay.
        press_surface_key('2', workspace, &conversation, &mut input, &mut state, &tx).await;
        assert!(!state.retro_left_out.contains_key(item));
        let painted = paint_surface(&mut state, &conversation, 100, 40);
        assert!(!painted.contains("with redactions") && !painted.contains("left out"));

        // A field-level change under a non-empty overlay still re-renders.
        press_surface_key('1', workspace, &conversation, &mut input, &mut state, &tx).await;
        let painted = paint_surface(&mut state, &conversation, 100, 40);
        assert!(painted.contains("[1] received items: left out"));
        press_surface_key('3', workspace, &conversation, &mut input, &mut state, &tx).await;
        let painted = paint_surface(&mut state, &conversation, 100, 40);
        assert!(
            painted.contains("[3] acknowledgement times: left out"),
            "{painted}"
        );
        assert!(painted.contains("[2] reply times: 3 replies, median 25m"));
        for unowned in ['0', '4', '9'] {
            press_surface_key(
                unowned,
                workspace,
                &conversation,
                &mut input,
                &mut state,
                &tx,
            )
            .await;
        }
        assert_eq!(
            overlay(&state),
            Some(set(&[RetroField::Received, RetroField::AckTimes]))
        );
        assert!(rx.try_recv().is_err());

        // `[s]` sends the overlay as the leave-out list, correlated.
        press_surface_key('s', workspace, &conversation, &mut input, &mut state, &tx).await;
        assert!(matches!(rx.try_recv().unwrap(),
            ClientFrame::ShareRecipientRetro { item_id, leave_out, decline: false }
                if item_id == item
                    && leave_out == vec![RetroField::Received, RetroField::AckTimes]));
        let action_key = (
            RecipientAction::ShareRetro,
            RecipientActionTarget::Item {
                item_id: item.to_owned(),
            },
        );
        assert!(matches!(
            state.recipient_actions.get(&action_key),
            Some(RecipientActionState::Pending { .. })
        ));
        let surface = state.recipient_surface.as_ref().unwrap();
        let notices = surface_action_notices(
            &state.recipient_actions,
            &surface.target,
            surface.node_id.as_deref(),
        )
        .map(|notice| notice.into_owned())
        .collect::<Vec<_>>();
        assert_eq!(
            notices,
            vec!["applying retrospective reply for ri_retro_keys…"]
        );
        // Pending suppresses all three retro acts, toggles included.
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert_eq!(actions[1], Some(RecipientSurfaceAction::Busy));
        assert!(actions[2].is_none() && actions[3].is_none() && actions[4].is_none());
        for duplicate in ['2', 's', 'n'] {
            press_surface_key(
                duplicate,
                workspace,
                &conversation,
                &mut input,
                &mut state,
                &tx,
            )
            .await;
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(
            overlay(&state),
            Some(set(&[RetroField::Received, RetroField::AckTimes]))
        );
        assert_eq!(input, "unfinished operator text");

        // Settling releases the overlay and the acts.
        apply_action_result(
            &mut state,
            &mut input,
            RecipientAction::ShareRetro,
            action_key.1.clone(),
            RecipientActionOutcome::Applied { reply: None },
        );
        assert!(state.retro_left_out.is_empty());
        assert!(!card(&conversation, &state).contains("left out"));
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert_eq!(actions[2], Some(RecipientSurfaceAction::RetroShare));

        // `[n]` declines with no fields even over a non-empty overlay, which
        // a refusal keeps for the retry.
        press_surface_key('2', workspace, &conversation, &mut input, &mut state, &tx).await;
        press_surface_key('n', workspace, &conversation, &mut input, &mut state, &tx).await;
        assert!(matches!(rx.try_recv().unwrap(),
            ClientFrame::ShareRecipientRetro { item_id, leave_out, decline: true }
                if item_id == item && leave_out.is_empty()));
        apply_action_result(
            &mut state,
            &mut input,
            RecipientAction::ShareRetro,
            action_key.1.clone(),
            RecipientActionOutcome::Refused {
                error: ProtocolError::ReadOnly,
            },
        );
        assert_eq!(overlay(&state), Some(set(&[RetroField::ReplyTimes])));

        // A failed enqueue releases the pending key, not the overlay.
        drop(rx);
        assert!(!queue_recipient_action(
            &mut state,
            &mut input,
            ClientFrame::ShareRecipientRetro {
                item_id: item.to_owned(),
                leave_out: vec![RetroField::ReplyTimes],
                decline: false,
            },
            &tx,
            false,
        ));
        assert!(matches!(state.recipient_actions.get(&action_key),
            Some(RecipientActionState::Completed { notice }) if !notice.is_empty()));
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert_eq!(actions[2], Some(RecipientSurfaceAction::RetroShare));
        assert_eq!(overlay(&state), Some(set(&[RetroField::ReplyTimes])));
        assert_eq!(input, "unfinished operator text");

        // The card leaving takes the overlay and the surface preview with it.
        conversation
            .peer_row_controls
            .get_mut("retro-row")
            .unwrap()
            .stage = PeerRowStage::Settled;
        prune_retro_left_out(&mut state, &conversation);
        assert!(state.retro_left_out.is_empty());
        let painted = paint_surface(&mut state, &conversation, 100, 40);
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
        use crate::adapters::tui::state::{RecipientSurfaceAccess, RecipientSurfaceTarget};
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
        crate::infrastructure::runtime::recipient_surface::open_received_surface(
            workspace.path(),
            None,
            you.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        let surface = state.recipient_surface.as_ref().unwrap();
        assert_eq!(surface.verdict_offer, VerdictOffer::AcceptOrDiscuss);
        // Writer, non-removed, no live draft: slots 0/1/2 per AC4(c).
        let actions = recipient_surface_actions(&state, &Conversation::default(), false);
        assert_eq!(actions[0], Some(RecipientSurfaceAction::DiscussConcerns));
        assert_eq!(actions[1], Some(RecipientSurfaceAction::Acknowledge));
        assert_eq!(actions[2], Some(RecipientSurfaceAction::Accept));

        state.recipient_surface.as_mut().unwrap().node_id = Some("verdict-node".to_owned());
        // Drafting: no verdict acts at all, the acknowledgement stays.
        for offer in [VerdictOffer::AcceptOrDiscuss, VerdictOffer::DiscussOnly] {
            state.recipient_surface.as_mut().unwrap().verdict_offer = offer;
            state.recipient_surface.as_mut().unwrap().item_state = Some("received");
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
            let actions = recipient_surface_actions(&state, &conversation, false);
            assert_eq!(actions[0], None, "Drafting offers no verdict");
            assert_eq!(actions[1], Some(RecipientSurfaceAction::Acknowledge));
            assert_eq!(actions[2], None, "Drafting offers no accept");
        }

        // Ready keeps Approve/Edit/Reject; Accept never survives it (M11).
        state.recipient_surface.as_mut().unwrap().verdict_offer = VerdictOffer::AcceptOrDiscuss;
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
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert_eq!(actions[2], Some(RecipientSurfaceAction::Approve));
        assert_eq!(actions[3], Some(RecipientSurfaceAction::Edit));
        assert_eq!(actions[4], Some(RecipientSurfaceAction::Reject));
        assert!(!actions.contains(&Some(RecipientSurfaceAction::Accept)));
        conversation
            .peer_row_controls
            .get_mut("verdict-row")
            .unwrap()
            .node_resolvable = false;
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert!(
            !actions.contains(&Some(RecipientSurfaceAction::Accept)),
            "Ready never offers Accept, including an orphan control"
        );

        // DiscussOnly offers concerns but no accept.
        state.recipient_surface.as_mut().unwrap().verdict_offer = VerdictOffer::DiscussOnly;
        let actions = recipient_surface_actions(&state, &Conversation::default(), false);
        assert_eq!(actions[0], Some(RecipientSurfaceAction::DiscussConcerns));
        assert_eq!(actions[2], None);

        // Offer None restores today's slot 0 by type.
        state.recipient_surface.as_mut().unwrap().verdict_offer = VerdictOffer::None;
        for (message_type, expected) in [
            (
                SemanticMessageType::ArchitectureUpdate,
                RecipientSurfaceAction::Discuss,
            ),
            (
                SemanticMessageType::BugReport,
                RecipientSurfaceAction::Discuss,
            ),
            (
                SemanticMessageType::Consultation,
                RecipientSurfaceAction::Respond,
            ),
        ] {
            state.recipient_surface.as_mut().unwrap().message_type = message_type;
            let actions = recipient_surface_actions(&state, &Conversation::default(), false);
            assert_eq!(actions[0], Some(expected), "{message_type:?}");
        }
        state.recipient_surface.as_mut().unwrap().message_type =
            SemanticMessageType::StoryAssignment;

        // Scope change received: slot 1 acknowledges (M12).
        let scope = append_story_list_item(
            &journal,
            "ri_verdict_scope",
            SemanticMessageType::ScopeChange,
            vec![StoryHolder::NoHolder],
        )
        .await;
        crate::infrastructure::runtime::recipient_surface::open_received_surface(
            workspace.path(),
            None,
            scope.item().as_str(),
            &mut state,
            RecipientSurfaceAccess::Writer,
            crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
        )
        .await
        .unwrap();
        assert_eq!(
            state.recipient_surface.as_ref().unwrap().verdict_offer,
            VerdictOffer::DiscussOnly,
            "a released list offers discuss only"
        );
        let actions = recipient_surface_actions(&state, &Conversation::default(), false);
        assert_eq!(actions[1], Some(RecipientSurfaceAction::Acknowledge));

        // Removed items and non-writer access offer nothing.
        state.recipient_surface.as_mut().unwrap().item_state = Some("removed");
        let actions = recipient_surface_actions(&state, &Conversation::default(), false);
        assert!(actions[0].is_none() && actions[1].is_none() && actions[2].is_none());
        state.recipient_surface.as_mut().unwrap().item_state = Some("received");
        for access in [
            RecipientSurfaceAccess::ReadOnly,
            RecipientSurfaceAccess::LocalOnly,
        ] {
            state.recipient_surface.as_mut().unwrap().access = access;
            assert!(
                recipient_surface_actions(&state, &Conversation::default(), false)
                    .iter()
                    .all(Option::is_none)
            );
        }
        state.recipient_surface.as_mut().unwrap().access = RecipientSurfaceAccess::Writer;
        assert!(
            recipient_surface_actions(&state, &Conversation::default(), true)
                .iter()
                .all(Option::is_none)
        );
        // A node target never offers verdict acts.
        state.recipient_surface.as_mut().unwrap().target =
            RecipientSurfaceTarget::Node("verdict-node".to_owned());
        let actions = recipient_surface_actions(&state, &Conversation::default(), false);
        assert!(!actions.contains(&Some(RecipientSurfaceAction::Accept)));
        assert!(!actions.contains(&Some(RecipientSurfaceAction::DiscussConcerns)));
    }

    #[tokio::test]
    async fn story19_22_verdict_keys_queue_frames_and_pending_suppresses_across_pages() {
        use crate::adapters::daemon::protocol::{RecipientAction, ReplyVerdict};
        use crate::adapters::tui::handlers::recipient_actions::action_is_pending;
        use crate::adapters::tui::state::{RecipientSurfaceAccess, RecipientSurfaceTarget};
        use crate::domain::models::{PeerRowStage, SemanticMessageType, StoryHolder};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
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
        let conversation = Conversation {
            messages: vec![peer_row("verdict-row", "incoming")],
            peer_row_controls: [("verdict-row".to_owned(), control)].into_iter().collect(),
            ..Default::default()
        };
        let mut state = TuiState::new(80, 24);
        let mut input = String::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let open = async |state: &mut TuiState| {
            crate::infrastructure::runtime::recipient_surface::open_received_surface(
                workspace.path(),
                Some(&conversation),
                address.item().as_str(),
                state,
                RecipientSurfaceAccess::Writer,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
        };
        open(&mut state).await.unwrap();
        let opened_id = address.item().as_str().to_owned();

        // `[y] Accept` queues the verdict frame with empty text.
        recipient_surface_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            workspace.path(),
            &conversation,
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        assert!(matches!(
            rx.try_recv().unwrap(),
            ClientFrame::ReplyToRecipientItem { item_id, text, verdict: Some(ReplyVerdict::Accept) }
                if item_id == opened_id && text.is_empty()
        ));
        assert!(action_is_pending(
            &state,
            RecipientAction::Reply,
            &RecipientSurfaceTarget::Item(address.clone()),
            None,
        ));

        // A pending Reply suppresses both verdict acts; the suppression
        // survives context selection and a newer page (cache re-render).
        let actions = recipient_surface_actions(&state, &conversation, false);
        assert_eq!(actions[0], None, "pending Reply suppresses DiscussConcerns");
        assert_eq!(actions[2], None, "pending Reply suppresses Accept");
        state.recipient_surface.as_mut().unwrap().context_selected = true;
        state.recipient_context_ids.insert(opened_id.clone());
        let suppressed = recipient_surface_actions(&state, &conversation, false);
        assert_eq!(suppressed[0], None);
        assert_eq!(suppressed[2], None);
        let painted = paint_received_surface(&mut state);
        assert!(painted.contains("[c] Stop using as context"));
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
        open(&mut state).await.unwrap();
        assert!(
            state
                .recipient_surface
                .as_ref()
                .unwrap()
                .content
                .contains("newer turn arrived")
        );
        let painted = paint_received_surface(&mut state);
        assert!(!painted.contains("[y] Accept"));
        assert!(!painted.contains("[d] Discuss concerns"));

        // A newer page repaints with the verdict acts restored once the
        // pending action clears — the widened u32 mask tracks both.
        state.recipient_actions.clear();
        open(&mut state).await.unwrap();
        let painted = paint_received_surface(&mut state);
        assert!(painted.contains("[y] Accept"));
        assert!(painted.contains("[d] Discuss concerns"));
        assert!(painted.contains("[a] Acknowledge"));

        // `[d] Discuss concerns` prefills the typed verdict line.
        recipient_surface_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            workspace.path(),
            &conversation,
            &mut input,
            &mut state,
            Some(&tx),
        )
        .await;
        assert_eq!(
            input,
            format!("/team reply --discuss {opened_id} "),
            "Discuss concerns prefills the typed verdict form"
        );
        assert!(state.recipient_surface.is_none());
        assert_eq!(state.focus, crate::domain::models::FocusState::Input);
    }

    #[test]
    fn story19_24_status_card_preview_forms_wrap_without_truncation() {
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
            let pending = pending_peer_response(&conversation).unwrap();
            let lines = peer_response_card_lines(&pending, false, 60);
            let text = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
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
                for story in stories {
                    assert!(joined.contains(story.key.as_str()), "{joined}");
                }
            }
            assert!(!joined.contains("private request body"));
        }
    }

    #[test]
    fn story19_25_retro_card_preview_forms_wrap_without_truncation() {
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
            let conversation = Conversation {
                messages: vec![peer_row("retro", "private request body")],
                peer_row_controls: [("retro".to_owned(), control)].into_iter().collect(),
                ..Default::default()
            };
            let mut pending = pending_peer_response(&conversation).unwrap();
            assert!(pending.retro_left_out.is_none());
            pending.retro_left_out = left_out;
            let text = peer_response_card_lines(&pending, false, 60)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
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
        assert_eq!(shared[1], "  Shares (last 14 days):");
        assert_eq!(
            shared[2],
            "  [1] received items: consultation 3 · story_assignment 2"
        );
        assert_eq!(shared[3], "  [2] reply times: 5 replies, median 25m");
        assert_eq!(shared[4], "  [3] acknowledgement times: none");
        assert_eq!(full[1], "[2] reply times: 5 replies, median 25m");
        let tail = joined(&shared[5..]);
        assert!(
            tail.starts_with(&format!("Not shared: {not_shared_tail}")),
            "{tail}"
        );
        assert!(shared[5].starts_with("  Not shared: message content,"));

        let left_out: RetroFieldSet = [RetroField::ReplyTimes].into_iter().collect();
        let redacted = render(
            RetroPreview {
                contribution: contribution.clone(),
                nothing_in_window: false,
            },
            Some(&left_out),
        );
        assert_eq!(redacted[2], shared[2]);
        assert_eq!(redacted[3], "  [2] reply times: left out");
        assert_eq!(redacted[4], shared[4]);
        assert!(redacted[5].starts_with("  Not shared: reply times · message content,"));
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
        assert_eq!(everything[2], "  [1] received items: left out");
        assert_eq!(everything[3], "  [2] reply times: left out");
        assert_eq!(everything[4], "  [3] acknowledgement times: left out");
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

        // Narrower than 60 columns the preview still wraps inside the card.
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
        let narrow =
            peer_response_card_lines(&pending_peer_response(&conversation).unwrap(), false, 40)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
        let preview = narrow
            .iter()
            .skip(1)
            .take_while(|line| line.starts_with("  "))
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
        let pending = pending_peer_response(&conversation).unwrap();
        assert!(pending.retro.is_none());
        let plain_text = joined(
            &peer_response_card_lines(&pending, false, 60)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        );
        assert!(!plain_text.contains("Shares"), "{plain_text}");
        assert!(!plain_text.contains("Not shared"), "{plain_text}");
    }

    #[test]
    fn story_card_summary_uses_control_only_at_floor_width() {
        use crate::adapters::tui::widgets::inline_card::render_bottom_anchored_decision_card;
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
            let pending = pending_peer_response(&conversation).unwrap();
            let lines = peer_response_card_lines(&pending, false, 58);
            let text = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
            if let Some(expected) = expected {
                assert_eq!(text[1], expected);
            } else {
                assert!(!text.iter().any(|line| line.contains(summary)));
            }
            let pane = ratatui::layout::Rect::new(0, 0, 60, 12);
            let mut buffer = ratatui::buffer::Buffer::empty(pane);
            render_bottom_anchored_decision_card(
                &mut buffer,
                lines,
                ratatui::style::Color::Reset,
                pane,
            );
            let rendered = (0..pane.height)
                .map(|y| {
                    (0..pane.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(expected) = expected {
                assert!(rendered.contains(expected));
            } else {
                assert!(!rendered.contains(summary));
            }
        }
    }

    /// Story 19.23 AC5(a,c): one compact host line under header + story summary.
    #[test]
    fn story19_23_card_selection_line_at_floor_width() {
        use crate::adapters::tui::widgets::inline_card::render_bottom_anchored_decision_card;
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
            let pending = pending_peer_response(&conversation).unwrap();
            let lines = peer_response_card_lines(&pending, false, 58);
            let text = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
            assert!(text[0].ends_with(" · nina"), "{text:?}");
            assert_eq!(text[1], "2 stories");
            match expected {
                Some(expected) => {
                    assert_eq!(text[2], expected);
                    assert_eq!(text.len(), 5, "{text:?}");
                }
                None => {
                    assert_eq!(text.len(), 4, "{text:?}");
                    assert_eq!(text[2], format!("Peer draft: {spoof}"));
                    assert!(!text.iter().any(|line| line == spoof), "{text:?}");
                }
            }
            let pane = ratatui::layout::Rect::new(0, 0, 60, 12);
            let mut buffer = ratatui::buffer::Buffer::empty(pane);
            render_bottom_anchored_decision_card(
                &mut buffer,
                lines,
                ratatui::style::Color::Reset,
                pane,
            );
            let rendered = (0..pane.height)
                .map(|y| {
                    (0..pane.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            if let Some(expected) = expected {
                assert!(
                    rendered.iter().any(|row| row.contains(expected)),
                    "{rendered:?}"
                );
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

    #[tokio::test]
    async fn story_assignment_surface_a_key_dispatches_ack_only_for_received_writer() {
        use crate::adapters::daemon::protocol::ClientFrame;
        use crate::adapters::tui::state::RecipientSurfaceAccess;
        use crate::adapters::tui::widgets::recipient_surface::RecipientSurfaceAction;
        use crate::domain::models::SemanticMessageType;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for message_type in [
            SemanticMessageType::StoryAssignment,
            SemanticMessageType::BugReport,
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let journal =
                crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
                    workspace.path(),
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
            let conversation = Conversation {
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
            let mut state = TuiState::new(80, 24);
            let mut input = String::new();
            let (tx, mut rx) = mpsc::unbounded_channel();

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
            assert!(actions.contains(&Some(RecipientSurfaceAction::Acknowledge)));
            recipient_surface_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                workspace.path(),
                &conversation,
                &mut input,
                &mut state,
                Some(&tx),
            )
            .await;
            assert!(
                rx.try_recv().is_err(),
                "opening/viewing a received story assignment does not acknowledge it"
            );

            recipient_surface_key(
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                workspace.path(),
                &conversation,
                &mut input,
                &mut state,
                Some(&tx),
            )
            .await;
            assert!(matches!(
                rx.try_recv(),
                Ok(ClientFrame::AcknowledgeRecipientItem { item_id })
                    if item_id == address.item().as_str()
            ));
            assert!(
                recipient_surface_actions(&state, &conversation, false)
                    .contains(&Some(RecipientSurfaceAction::Context)),
                "after acknowledgement, the open surface still offers [c] Use as context"
            );
            recipient_surface_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
                workspace.path(),
                &conversation,
                &mut input,
                &mut state,
                Some(&tx),
            )
            .await;
            assert!(matches!(
                rx.try_recv(),
                Ok(ClientFrame::SetRecipientContext {
                    item_id,
                    selected: true
                }) if item_id == address.item().as_str()
            ));
            crate::adapters::tui::handlers::recipient_actions::apply_action_result(
                &mut state,
                &mut input,
                crate::adapters::daemon::protocol::RecipientAction::ContextSelect,
                crate::adapters::daemon::protocol::RecipientActionTarget::Item {
                    item_id: address.item().as_str().to_owned(),
                },
                crate::adapters::daemon::protocol::RecipientActionOutcome::ContextSelection {
                    selected: true,
                    changed: true,
                },
            );
            recipient_surface_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
                workspace.path(),
                &conversation,
                &mut input,
                &mut state,
                Some(&tx),
            )
            .await;
            assert!(matches!(
                rx.try_recv(),
                Ok(ClientFrame::SetRecipientContext {
                    item_id,
                    selected: false
                }) if item_id == address.item().as_str()
            ));

            crate::infrastructure::runtime::recipient_surface::open_received_surface(
                workspace.path(),
                Some(&conversation),
                address.item().as_str(),
                &mut state,
                RecipientSurfaceAccess::ReadOnly,
                crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            .unwrap();
            assert!(
                state
                    .recipient_surface
                    .as_ref()
                    .is_some_and(|surface| surface.can_toggle_target),
                "read-only item surface retains its [Tab] View item control"
            );
            assert!(
                !recipient_surface_actions(&state, &conversation, true)
                    .contains(&Some(RecipientSurfaceAction::Acknowledge)),
                "read-only item surfaces do not offer acknowledgement"
            );

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
            if let Some(surface) = state.recipient_surface.as_mut() {
                surface.item_state = Some("removed");
            }
            assert!(
                !recipient_surface_actions(&state, &conversation, false)
                    .contains(&Some(RecipientSurfaceAction::Acknowledge)),
                "removed items do not offer acknowledgement"
            );
        }
    }

    #[tokio::test]
    async fn story19_20_bug_report_discuss_survives_settled_or_absent_control() {
        use crate::adapters::tui::state::RecipientSurfaceAccess;
        use crate::adapters::tui::widgets::recipient_surface::RecipientSurfaceAction;
        use crate::domain::models::{PeerRowControl, PeerRowStage, SemanticMessageType};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
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
            for access in [
                RecipientSurfaceAccess::Writer,
                RecipientSurfaceAccess::ReadOnly,
                RecipientSurfaceAccess::LocalOnly,
            ] {
                let mut state = TuiState::new(80, 24);
                crate::infrastructure::runtime::recipient_surface::open_received_surface(
                    workspace.path(),
                    Some(&conversation),
                    address.item().as_str(),
                    &mut state,
                    access,
                    crate::infrastructure::runtime::recipient_view::ReceivedThreadWindow::Newest,
                )
                .await
                .unwrap();
                let actions = recipient_surface_actions(&state, &conversation, false);
                if access != RecipientSurfaceAccess::Writer {
                    assert!(actions.iter().all(Option::is_none));
                    continue;
                }
                assert!(matches!(
                    actions[1],
                    Some(RecipientSurfaceAction::Acknowledge)
                ));
                if stage == Some(PeerRowStage::Drafting) {
                    assert!(actions[0].is_none());
                    continue;
                }
                assert!(matches!(actions[0], Some(RecipientSurfaceAction::Discuss)));
                let (tx, mut rx) = mpsc::unbounded_channel();
                let mut input = String::new();
                recipient_surface_key(
                    KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                    workspace.path(),
                    &conversation,
                    &mut input,
                    &mut state,
                    Some(&tx),
                )
                .await;
                assert_eq!(input, "/team reply ri_bug_discuss ");
                assert!(rx.try_recv().is_err(), "Discuss composes, it does not send");
                assert!(state.recipient_surface.is_none());
            }
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
        paint_surface(state, &Conversation::default(), 80, 24)
    }

    /// Paints through the production overlay lookup; the surface's own cache
    /// persists across calls, as across frames.
    fn paint_surface(
        state: &mut TuiState,
        conversation: &Conversation,
        width: u16,
        height: u16,
    ) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let actions = recipient_surface_actions(state, conversation, false);
        terminal
            .draw(|frame| {
                let area = frame.area();
                let retro_left_out = &state.retro_left_out;
                let retro = state
                    .recipient_surface
                    .as_ref()
                    .and_then(|surface| retro_overlay(conversation, surface, retro_left_out));
                render_recipient_surface(
                    frame,
                    area,
                    state.recipient_surface.as_mut().unwrap(),
                    &state.recipient_actions,
                    &actions,
                    retro,
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
    fn latest_retractable_skips_human_decided_rows() {
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
            latest_retractable(&conversation).map(|row| row.id),
            Some(automatic.id),
            "M07: finder must skip newer human-decided, evidence, RAP and unsettled rows"
        );
        conversation.messages.remove(0);
        assert!(
            latest_retractable(&conversation).is_none(),
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

    fn snapshot_with(transcript: Vec<ChatMessage>, blocked: usize) -> AttachSnapshot {
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
        terminal
            .draw(|frame| {
                chat_pane::render_attached(
                    frame,
                    frame.area(),
                    conversation,
                    streaming,
                    0,
                    true,
                    &theme,
                    &mut tab,
                    &HashMap::new(),
                    &BTreeMap::new(),
                    None,
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
        use crate::adapters::tui::handlers::team_command::TRUST_REFUSAL;
        use crossterm::event::KeyCode;

        for read_only in [false, true] {
            for line in ["/team trust", "/team trust x"] {
                assert!(matches!(
                    attached_team_line(line),
                    AttachedTeamLine::Refused(message) if message == TRUST_REFUSAL
                ));
                let mut state = TuiState::new(80, 24);
                let (tx, mut rx) = mpsc::unbounded_channel();
                let mut requests = AttachedTeamRequests::default();
                let mut input = line.to_owned();
                assert!(matches!(
                    attached_composer_key_with_team_requests(
                        KeyCode::Enter,
                        read_only,
                        &mut input,
                        &mut state,
                        &tx,
                        &mut requests,
                    ),
                    ComposerOutcome::Handled
                ));
                assert!(input.is_empty());
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
            let mut input = command.to_owned();
            assert!(matches!(
                attached_composer_key_with_team_requests(
                    KeyCode::Enter,
                    true,
                    &mut input,
                    &mut state,
                    &tx,
                    &mut requests
                ),
                ComposerOutcome::FrameSent
            ));
            assert!(matches!(
                rx.try_recv(),
                Ok(ClientFrame::TeamBoard { .. } | ClientFrame::TeamStatus { .. })
            ));
            assert!(input.is_empty());
        }
        for command in ["/team send a hello", "/team untrust a"] {
            let mut input = command.to_owned();
            assert!(matches!(
                attached_composer_key_with_team_requests(
                    KeyCode::Enter,
                    true,
                    &mut input,
                    &mut state,
                    &tx,
                    &mut requests
                ),
                ComposerOutcome::Handled
            ));
            assert_eq!(input, command);
            assert!(rx.try_recv().is_err());
        }
        let draft = "ordinary read-only draft";
        let mut input = draft.to_owned();
        assert!(matches!(
            attached_composer_key_with_team_requests(
                KeyCode::Enter,
                true,
                &mut input,
                &mut state,
                &tx,
                &mut requests
            ),
            ComposerOutcome::Handled
        ));
        assert_eq!(input, draft, "read-only submission retains the draft");
        assert!(rx.try_recv().is_err(), "read-only draft is never forwarded");
    }

    #[test]
    fn attached_paste_keeps_multiline_team_send_body_intact() {
        let mut input = "/team send --type story_assignment --subject sprint-7 jun-dev ".to_owned();
        let body = "line one\n  indented line\n\nfinal line";
        append_attached_paste(&mut input, body);
        let AttachedTeamLine::Frame(ClientFrame::TeamSend {
            request,
            recipients,
            text,
            message_type,
            subject,
            stories: None,
            holders_of: None,
        }) = attached_team_line_for_request(&input, "paste-1".into())
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
    /// draw; its read-only path remains reachable before the composer guard,
    /// and the observer never touches a turn.
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
                    &HashMap::new(),
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
        attached_composer_key_with_team_requests(
            KeyCode::Enter,
            reader,
            &mut line.to_owned(),
            state,
            tx,
            requests,
        )
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
                assert_eq!(
                    state.feedback_blocks["team-thread"].message,
                    crate::adapters::tui::handlers::team_command::TEAM_THREAD_READING
                );
            } else {
                assert!(rx.try_recv().is_err());
                assert!(
                    matches!(state.status, StatusState::Flash { message, .. } if message == flash)
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
            "  ● B  delivered"
        );
        assert!(requests.pending.is_empty());
    }

    #[test]
    fn retract_card_key_matrix_owns_focus_and_double_y_sends_once() {
        for armed in [false, true] {
            for key in [
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            ] {
                let mut state = TuiState::new(80, 24);
                let mut requests = AttachedTeamRequests::default();
                let (tx, mut rx) = mpsc::unbounded_channel();
                preview(&mut state, &mut requests, armed, false, &tx);
                assert_eq!(
                    state.pending_team_retract.as_ref().unwrap().card,
                    "Exact card body"
                );
                assert!(attached_team_retract_key(
                    &mut state,
                    &mut requests,
                    key,
                    &tx
                ));
                let expected = match key.code {
                    KeyCode::Char('y') if armed => Some(true),
                    KeyCode::Char('n') | KeyCode::Esc => Some(false),
                    _ => None,
                };
                if let Some(confirm) = expected {
                    assert!(
                        matches!(rx.try_recv().unwrap(), ClientFrame::TeamRetractDecision {
                        request, confirm: found } if request == "retract" && found == confirm)
                    );
                    assert!(state.pending_team_retract.is_none());
                    assert!(requests.open_retract.is_none());
                    if confirm {
                        assert!(matches!(
                            requests.pending["retract"],
                            AttachedTeamRequest::Retract { .. }
                        ));
                        assert!(!attached_team_retract_key(
                            &mut state,
                            &mut requests,
                            key,
                            &tx
                        ));
                    } else {
                        assert!(requests.pending.is_empty());
                    }
                } else {
                    assert!(state.pending_team_retract.is_some());
                    assert_eq!(requests.open_retract.as_deref(), Some("retract"));
                }
                assert!(rx.try_recv().is_err());
            }
        }
    }

    #[test]
    fn preview_busy_declines_and_second_retract_never_replaces_pending_card() {
        for busy in [false, true] {
            let mut state = TuiState::new(80, 24);
            let mut requests = AttachedTeamRequests::default();
            let (tx, mut rx) = mpsc::unbounded_channel();
            preview(&mut state, &mut requests, true, busy, &tx);
            if busy {
                assert!(state.pending_team_retract.is_none());
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    ClientFrame::TeamRetractDecision { confirm: false, .. }
                ));
                assert!(
                    state.feedback_blocks["team-retract"]
                        .message
                        .starts_with("Another confirmation")
                );
            } else {
                submit(
                    &mut state,
                    &mut requests,
                    "/team retract B ri_other",
                    false,
                    &tx,
                );
                assert!(rx.try_recv().is_err());
                assert_eq!(state.pending_team_retract.as_ref().unwrap().item_id, "ri_a");
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
        assert_eq!(state.feedback_blocks["team-thread"].message, "latest");
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
