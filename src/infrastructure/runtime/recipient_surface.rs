use anyhow::Result;
use std::path::Path;

use super::recipient_view::{ReceivedThreadWindow, load_received_thread};
use crate::adapters::tui::state::{
    RecipientSurface, RecipientSurfaceAccess, RecipientSurfaceTarget, TuiState,
};
use crate::domain::models::{Conversation, FocusState};

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

pub(crate) fn reconcile_item_surface(state: &mut TuiState, conversation: &Conversation) {
    let Some(surface) = state.recipient_surface.as_mut() else {
        return;
    };
    let RecipientSurfaceTarget::Item(address) = &surface.target else {
        return;
    };
    surface.node_id = item_node_id(conversation, address);
    surface.can_toggle_target = surface.node_id.is_some();
    surface.cached_width = 0;
}

pub(crate) async fn open_received_surface(
    workspace: &Path,
    conversation: Option<&Conversation>,
    item_id: &str,
    state: &mut TuiState,
    access: RecipientSurfaceAccess,
    window: ReceivedThreadWindow,
) -> Result<()> {
    let page = load_received_thread(workspace, item_id, window).await?;
    let context_selected = state.recipient_context_ids.contains(item_id);
    let node_id =
        conversation.and_then(|conversation| item_node_id(conversation, &page.opened_address));
    let old = state.recipient_surface.take();
    let (prior_focus, results) = match old {
        Some(surface) => (
            surface.prior_focus,
            if matches!(&surface.target, RecipientSurfaceTarget::Item(address) if address == &page.opened_address)
            {
                surface.results
            } else {
                Vec::new()
            },
        ),
        None => (
            std::mem::replace(&mut state.focus, FocusState::Chat),
            Vec::new(),
        ),
    };
    state.focus = FocusState::Chat;
    state.recipient_surface = Some(RecipientSurface {
        target: RecipientSurfaceTarget::Item(page.opened_address),
        access,
        can_toggle_target: node_id.is_some(),
        root_id: Some(page.root_id),
        first_non_pinned_ordinal: page.first_non_pinned_ordinal,
        last_non_pinned_ordinal: page.last_non_pinned_ordinal,
        omitted_before: page.omitted_before,
        omitted_after: page.omitted_after,
        node_id,
        item_state: Some(page.item_state),
        context_available: page.context_available,
        context_selected,
        prior_focus,
        scroll_offset: 0,
        viewport_rows: 0,
        content: page.content,
        message_type: page.message_type,
        cached_width: 0,
        cached_lines: Vec::new(),
        cached_header: Vec::new(),
        cached_footer: Vec::new(),
        cached_action_mask: 0,
        results,
    });
    state.needs_redraw = true;
    Ok(())
}

pub(crate) async fn navigate_received_surface(
    workspace: &Path,
    conversation: Option<&Conversation>,
    state: &mut TuiState,
    earlier: bool,
) -> Result<()> {
    let Some(surface) = state.recipient_surface.as_ref() else {
        return Ok(());
    };
    let RecipientSurfaceTarget::Item(address) = &surface.target else {
        return Ok(());
    };
    let window = if earlier && surface.omitted_before > 0 {
        ReceivedThreadWindow::Earlier {
            before: surface.first_non_pinned_ordinal.unwrap_or(usize::MAX),
        }
    } else if !earlier && surface.omitted_after > 0 {
        ReceivedThreadWindow::Newer {
            after: surface.last_non_pinned_ordinal.unwrap_or(0),
        }
    } else {
        return Ok(());
    };
    let item_id = address.item().as_str().to_owned();
    let access = surface.access;
    let result =
        open_received_surface(workspace, conversation, &item_id, state, access, window).await;
    if let Err(error) = &result {
        crate::adapters::tui::handlers::recipient_actions::show_notice(
            state,
            None,
            error.to_string(),
        );
    }
    result
}
