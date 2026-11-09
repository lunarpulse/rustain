//! Openers of the thread frame (Story 21.8, A3). Both hosts read the bounded page the same
//! way and hand the payload to the presentation reducer; opening never writes base focus,
//! so `Esc` restores exactly what the frame covered.

use anyhow::Result;
use std::path::Path;

use super::recipient_view::{ReceivedThreadWindow, load_received_thread};
use crate::adapters::tui::presentation::reducer;
use crate::adapters::tui::state::{ThreadAccess, ThreadInspectorState, ThreadTarget, TuiState};
use crate::adapters::tui::thread_inspector;
use crate::domain::models::Conversation;

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

/// A pinned-response change may have moved the opened item's node: refresh the join.
pub(crate) fn reconcile_item_thread(state: &mut TuiState, conversation: &Conversation) {
    let Some(inspector) = state.thread_inspector.as_mut() else {
        return;
    };
    let ThreadTarget::Item(address) = &inspector.target;
    inspector.node_id = item_node_id(conversation, address);
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
    // Outcomes belong to the item: a page of the same item keeps them, another item starts clean.
    let results = match state.thread_inspector.as_mut() {
        Some(old) if matches!(&old.target, ThreadTarget::Item(address) if address == &page.opened_address) => {
            std::mem::take(&mut old.results)
        }
        _ => Vec::new(),
    };
    let mut inspector = ThreadInspectorState::new(
        ThreadTarget::Item(page.opened_address),
        access,
        page.message_type,
    );
    inspector.root_id = Some(page.root_id);
    inspector.first_non_pinned_ordinal = page.first_non_pinned_ordinal;
    inspector.last_non_pinned_ordinal = page.last_non_pinned_ordinal;
    inspector.omitted_before = page.omitted_before;
    inspector.omitted_after = page.omitted_after;
    inspector.node_id = node_id;
    inspector.item_state = Some(page.item_state);
    inspector.context_available = page.context_available;
    inspector.context_selected = state.recipient_context_ids.contains(item_id);
    inspector.content = page.content;
    inspector.verdict_offer = page.verdict_offer;
    inspector.results = results;
    reducer::open_thread(state, inspector, thread_inspector::title(page.message_type));
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
    let ThreadTarget::Item(address) = &inspector.target;
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
