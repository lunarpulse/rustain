//! Openers of the thread frame (Story 21.8, A3). Both hosts read the bounded page the same
//! way and hand the payload to the presentation reducer; opening never writes base focus,
//! so `Esc` restores exactly what the frame covered.

use anyhow::{Result, anyhow};
use std::path::Path;

use super::recipient_view::{ReceivedThreadWindow, append_safe_body, load_received_thread};
use crate::adapters::tui::presentation::markdown_body::BodyMode;
use crate::adapters::tui::presentation::reducer;
use crate::adapters::tui::state::{ThreadAccess, ThreadInspectorState, ThreadTarget, TuiState};
use crate::adapters::tui::thread_inspector::{self, pinned_response};
use crate::domain::models::{ChatMessage, Conversation, PeerRowControl};

pub(crate) fn item_node_id(
    conversation: &Conversation,
    address: &crate::domain::models::ItemAddress,
) -> Option<String> {
    conversation
        .peer_row_controls
        .values()
        .find(|control| control.item_address.as_ref() == Some(address))
        .map(|control| control.node_id.clone())
}

/// A pinned-response change may have moved the opened item's node (refresh the join) or the
/// text of the response view (rebuild it).
pub(crate) fn reconcile_item_thread(state: &mut TuiState, conversation: &Conversation) {
    let Some(inspector) = state.thread_inspector.as_mut() else {
        return;
    };
    if let ThreadTarget::Node(node) = inspector.target.clone() {
        if let Some((message, control)) = pinned_response(conversation, &node) {
            let (content, segments, turns) = response_page(message, control);
            inspector.content = content;
            inspector.segments = segments;
            inspector.turns = turns;
            inspector.can_toggle = control.item_address.is_some();
            inspector.invalidate();
        }
        return;
    }
    let Some(address) = inspector.item_address() else {
        return;
    };
    inspector.node_id = item_node_id(conversation, address);
    inspector.can_toggle = inspector.node_id.is_some();
    inspector.invalidate();
}

/// Open (or retarget) the one thread frame on a received item.
pub async fn open_received(
    workspace: &Path,
    conversation: Option<&Conversation>,
    item_id: &str,
    state: &mut TuiState,
    access: ThreadAccess,
    window: ReceivedThreadWindow,
) -> Result<()> {
    let page = load_received_thread(workspace, item_id, window).await?;
    let node_id =
        conversation.and_then(|conversation| item_node_id(conversation, &page.opened_address));
    // The body mode and the kept filter belong to the item: a page of the same item keeps
    // them, another item starts clean.
    let same = state.thread_inspector.take_if(
        |old| matches!(&old.target, ThreadTarget::Item(address) if address == &page.opened_address),
    );
    let (mode, search) = same.map_or_else(
        || (BodyMode::Rendered, Default::default()),
        |old| (old.mode, old.search),
    );
    let mut inspector = ThreadInspectorState::new(
        ThreadTarget::Item(page.opened_address),
        access,
        page.message_type,
    );
    inspector.mode = mode;
    inspector.search = search;
    inspector.root_id = Some(page.root_id);
    inspector.first_non_pinned_ordinal = page.first_non_pinned_ordinal;
    inspector.last_non_pinned_ordinal = page.last_non_pinned_ordinal;
    inspector.omitted_before = page.omitted_before;
    inspector.omitted_after = page.omitted_after;
    inspector.can_toggle = node_id.is_some();
    inspector.node_id = node_id;
    inspector.item_state = Some(page.item_state);
    inspector.context_available = page.context_available;
    inspector.context_selected = state.recipient_context_ids.contains(item_id);
    inspector.content = page.content;
    inspector.segments = page.segments;
    inspector.turns = page.turns;
    inspector.original = page.original;
    inspector.verdict_offer = page.verdict_offer;
    reducer::open_thread(state, inspector, thread_inspector::title(page.message_type));
    Ok(())
}

/// The response view of a pinned peer node: its sender, story summary and the response text
/// the operator would see in the transcript row. The text is peer-derived and escaped once.
fn response_page(
    message: &ChatMessage,
    control: &PeerRowControl,
) -> (
    String,
    Vec<crate::adapters::tui::thread_body::ThreadSegment>,
    Vec<crate::adapters::tui::thread_body::TurnMark>,
) {
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
    let mut bodies = Vec::new();
    let mut turn_starts = Vec::new();
    if super::recipient_view::safe_display_len(body)
        > crate::domain::services::recipient_thread_bounds::MAX_THREAD_TURN_BYTES
            .saturating_sub(content.len())
    {
        content.push_str("      turn content withheld · exceeds 448 KiB");
    } else {
        turn_starts.push((content.len(), false));
        bodies.push(append_safe_body(&mut content, body));
    }
    let (segments, turns) =
        crate::adapters::tui::thread_body::split_segments(&content, &bodies, &turn_starts);
    (content, segments, turns)
}

/// Open (or retarget) the one thread frame on the response view of `node`. Writer actions
/// do not exist here: the node's draft decision is 21-6's frame (`o` opens it).
pub fn open_node(
    conversation: &Conversation,
    node: &str,
    state: &mut TuiState,
    access: ThreadAccess,
) -> Result<()> {
    let (message, control) = pinned_response(conversation, node)
        .ok_or_else(|| anyhow!("response already settled — no action applied"))?;
    let (content, segments, turns) = response_page(message, control);
    let mut inspector = ThreadInspectorState::new(
        ThreadTarget::Node(node.to_owned()),
        access,
        control.message_type,
    );
    if let Some(old) = state
        .thread_inspector
        .as_ref()
        .filter(|old| matches!(&old.target, ThreadTarget::Node(n) if n == node))
    {
        inspector.mode = old.mode;
    }
    inspector.node_id = Some(node.to_owned());
    inspector.can_toggle = control.item_address.is_some();
    inspector.content = content;
    inspector.segments = segments;
    inspector.turns = turns;
    reducer::open_thread(state, inspector, thread_inspector::node_title());
    Ok(())
}

/// `p`/`l`: the earlier or later bounded page of the opened item (root and item retained, the
/// viewer back at the top).
pub async fn navigate(
    workspace: &Path,
    conversation: Option<&Conversation>,
    state: &mut TuiState,
    earlier: bool,
) -> Result<()> {
    let Some(inspector) = state.thread_inspector.as_ref() else {
        return Ok(());
    };
    let Some(address) = inspector.item_address() else {
        return Ok(());
    };
    let window = if earlier && inspector.omitted_before > 0 {
        ReceivedThreadWindow::Earlier {
            before: inspector.first_non_pinned_ordinal.unwrap_or(usize::MAX),
        }
    } else if !earlier && inspector.omitted_after > 0 {
        ReceivedThreadWindow::Newer {
            after: inspector.last_non_pinned_ordinal.unwrap_or(0),
        }
    } else {
        return Ok(());
    };
    let item_id = address.item().as_str().to_owned();
    let access = inspector.access;
    let result = open_received(workspace, conversation, &item_id, state, access, window).await;
    if let Err(error) = &result {
        crate::adapters::tui::handlers::recipient_actions::show_notice(
            state,
            None,
            error.to_string(),
        );
    }
    result
}
