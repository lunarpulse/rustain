use crate::adapters::daemon::protocol::{
    ClientFrame, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
};
use crate::adapters::tui::state::{RecipientActionState, ThreadTarget, TuiState};
use crate::domain::models::FeedbackLevel;
use crate::domain::services::transparency::{MAX_PEER_ID_BYTES, sanitize_disclosable};
use std::{borrow::Cow, collections::BTreeMap};

pub(crate) fn action_key(frame: &ClientFrame) -> Option<(RecipientAction, RecipientActionTarget)> {
    let (action, item_id) = match frame {
        ClientFrame::ReplyToRecipientItem { item_id, .. } => (RecipientAction::Reply, item_id),
        ClientFrame::DeferRecipientItem { item_id } => (RecipientAction::Defer, item_id),
        ClientFrame::ShareRecipientStatus { item_id, .. } => {
            (RecipientAction::ShareStatus, item_id)
        }
        // Story 19.25 `AC3` — Share and Decline hold one pending key.
        ClientFrame::ShareRecipientRetro { item_id, .. } => (RecipientAction::ShareRetro, item_id),
        ClientFrame::AcknowledgeRecipientItem { item_id } => {
            (RecipientAction::Acknowledge, item_id)
        }
        ClientFrame::SetRecipientContext { item_id, selected } => (
            if *selected {
                RecipientAction::ContextSelect
            } else {
                RecipientAction::ContextRemove
            },
            item_id,
        ),
        ClientFrame::ResolvePeerDraft { node, .. } => {
            return Some((
                RecipientAction::Response,
                RecipientActionTarget::Node {
                    node_id: node.clone(),
                },
            ));
        }
        _ => return None,
    };
    Some((
        action,
        RecipientActionTarget::Item {
            item_id: item_id.clone(),
        },
    ))
}

pub(crate) fn pending_composition(state: &TuiState) -> Option<&str> {
    state
        .recipient_actions
        .values()
        .find_map(|action| match action {
            RecipientActionState::Pending { composed_input, .. } => composed_input.as_deref(),
            RecipientActionState::Completed { .. } => None,
        })
}

pub(crate) fn begin_action(state: &mut TuiState, frame: &ClientFrame, composed: bool) -> bool {
    let Some(key) = action_key(frame) else {
        return true;
    };
    if state
        .recipient_actions
        .get(&key)
        .is_some_and(|action| matches!(action, RecipientActionState::Pending { .. }))
    {
        return false;
    }
    let visible = action_target_is_visible(state, &key.1)
        || state.recipient_actions.iter().any(|((_, target), phase)| {
            matches!(phase, RecipientActionState::Completed { .. })
                && action_target_is_visible(state, target)
        });
    state
        .recipient_actions
        .retain(|_, action| matches!(action, RecipientActionState::Pending { .. }));
    let pinned_compose = if composed {
        state.pinned_peer_compose.take()
    } else {
        None
    };
    let composed_input = composed.then(|| {
        state.cursor_position = 0;
        std::mem::take(&mut state.input_buffer)
    });
    state.recipient_actions.insert(
        key,
        RecipientActionState::Pending {
            composed_input,
            pinned_compose,
        },
    );
    refresh_action_notices(state, visible);
    true
}

pub(crate) fn action_is_pending(
    state: &TuiState,
    action: RecipientAction,
    target: &ThreadTarget,
    node: Option<&str>,
) -> bool {
    state.recipient_actions.iter().any(|((pending_action, pending_target), phase)| {
        matches!(phase, RecipientActionState::Pending { .. })
            && *pending_action == action
            && match pending_target {
                RecipientActionTarget::Item { item_id } => matches!(target, ThreadTarget::Item(address) if address.item().as_str() == item_id),
                RecipientActionTarget::Node { node_id } => node == Some(node_id.as_str()),
                // A `/team` request key never names a recipient surface.
                RecipientActionTarget::Request { .. } => false,
            }
    })
}

pub(crate) fn apply_action_result(
    state: &mut TuiState,
    action: RecipientAction,
    target: RecipientActionTarget,
    outcome: RecipientActionOutcome,
) {
    let key = (action, target);
    // Story 19.25 `AC3` — a settled retrospective act releases this client's
    // leave-out overlay; a refusal keeps it so a retry shares no more.
    if action == RecipientAction::ShareRetro
        && matches!(
            outcome,
            RecipientActionOutcome::Applied { .. } | RecipientActionOutcome::Idempotent
        )
        && let RecipientActionTarget::Item { item_id } = &key.1
    {
        state.retro_left_out.remove(item_id);
    }
    let pending = state.recipient_actions.remove(&key);
    let target = &key.1;
    let (submitted, pinned_compose) = pending
        .and_then(|phase| match phase {
            RecipientActionState::Pending {
                composed_input,
                pinned_compose,
            } => Some((composed_input, pinned_compose)),
            RecipientActionState::Completed { .. } => None,
        })
        .unwrap_or((None, None));
    let id = sanitize_disclosable(target_id(target), MAX_PEER_ID_BYTES);
    let message = match outcome {
        RecipientActionOutcome::Applied { reply: Some(reply) } => format!(
            "reply recorded for {} as {} — the asker sees it when they read the thread",
            sanitize_disclosable(&reply.item_id, MAX_PEER_ID_BYTES),
            sanitize_disclosable(&reply.turn_id, MAX_PEER_ID_BYTES),
        ),
        RecipientActionOutcome::Applied { reply: None } => {
            if action == RecipientAction::Acknowledge {
                acknowledge_surface_item(state, target);
                format!(
                    "acknowledgement recorded for {id} — receipt only; does not answer or select context"
                )
            } else {
                format!("response applied for {id}")
            }
        }
        RecipientActionOutcome::ContextSelection { selected, changed } => {
            if let RecipientActionTarget::Item { item_id } = target {
                if selected {
                    state.recipient_context_ids.insert(item_id.clone());
                } else {
                    state.recipient_context_ids.remove(item_id);
                }
                if let Some(inspector) = state.thread_inspector.as_mut()
                    && matches!(&inspector.target, ThreadTarget::Item(address) if address.item().as_str() == item_id)
                {
                    inspector.context_selected = selected;
                    inspector.invalidate();
                }
            }
            if selected {
                format!(
                    "item {id} selected as peer context for this daemon conversation — clears on /clear or daemon restart; view and acknowledgement do not select it"
                )
            } else if changed {
                format!("item {id} removed from this daemon conversation's peer context")
            } else {
                format!("item {id} is not selected as peer context for this daemon conversation")
            }
        }
        RecipientActionOutcome::Idempotent => {
            if action == RecipientAction::Acknowledge {
                acknowledge_surface_item(state, target);
                format!(
                    "acknowledgement already recorded for {id} — receipt only; no new action applied"
                )
            } else if let Some(text) = submitted {
                state.cursor_position = text.chars().count();
                state.input_buffer = text;
                state.pinned_peer_compose = pinned_compose;
                "response already settled — your text was not sent".to_owned()
            } else {
                "response already settled — no action applied".to_owned()
            }
        }
        RecipientActionOutcome::Refused { error } => {
            if let Some(text) = submitted {
                state.cursor_position = text.chars().count();
                state.input_buffer = text;
                state.pinned_peer_compose = pinned_compose;
            }
            format!("[daemon error] {error}")
        }
    };
    let visible = action_target_is_visible(state, target);
    state
        .recipient_actions
        .insert(key, RecipientActionState::Completed { notice: message });
    refresh_action_notices(state, visible);
}

pub(crate) fn apply_context_state(
    state: &mut TuiState,
    ids: Vec<String>,
    inventory: String,
    show_inventory: bool,
) {
    state.recipient_context_ids = ids.into_iter().collect();
    if let Some(inspector) = state.thread_inspector.as_mut() {
        let ThreadTarget::Item(address) = &inspector.target;
        let selected = state
            .recipient_context_ids
            .contains(address.item().as_str());
        if inspector.context_selected != selected {
            inspector.context_selected = selected;
            inspector.invalidate();
        }
    }
    if show_inventory {
        crate::adapters::tui::views::show_view(
            &mut state.feedback_blocks,
            crate::adapters::tui::views::ViewKind::RecipientOutcome,
            "recipient-context",
            FeedbackLevel::Info,
            inventory,
        );
    }
    state.needs_redraw = true;
}

pub(crate) fn abort_unsent_action(state: &mut TuiState, frame: ClientFrame, message: String) {
    let key = match frame {
        ClientFrame::ReplyToRecipientItem { item_id, .. } => (
            RecipientAction::Reply,
            RecipientActionTarget::Item { item_id },
        ),
        ClientFrame::DeferRecipientItem { item_id } => (
            RecipientAction::Defer,
            RecipientActionTarget::Item { item_id },
        ),
        ClientFrame::ShareRecipientStatus { item_id, .. } => (
            RecipientAction::ShareStatus,
            RecipientActionTarget::Item { item_id },
        ),
        ClientFrame::ShareRecipientRetro { item_id, .. } => (
            RecipientAction::ShareRetro,
            RecipientActionTarget::Item { item_id },
        ),
        ClientFrame::AcknowledgeRecipientItem { item_id } => (
            RecipientAction::Acknowledge,
            RecipientActionTarget::Item { item_id },
        ),
        ClientFrame::SetRecipientContext { item_id, selected } => (
            if selected {
                RecipientAction::ContextSelect
            } else {
                RecipientAction::ContextRemove
            },
            RecipientActionTarget::Item { item_id },
        ),
        ClientFrame::ResolvePeerDraft { node, .. } => (
            RecipientAction::Response,
            RecipientActionTarget::Node { node_id: node },
        ),
        _ => return,
    };
    if let Some(RecipientActionState::Pending {
        composed_input: Some(text),
        pinned_compose,
    }) = state.recipient_actions.remove(&key)
    {
        state.cursor_position = text.chars().count();
        state.input_buffer = text;
        state.pinned_peer_compose = pinned_compose;
    }
    let visible = action_target_is_visible(state, &key.1);
    state
        .recipient_actions
        .insert(key, RecipientActionState::Completed { notice: message });
    refresh_action_notices(state, visible);
}

fn action_notice<'a>(
    action: RecipientAction,
    target: &RecipientActionTarget,
    phase: &'a RecipientActionState,
) -> Cow<'a, str> {
    match phase {
        RecipientActionState::Completed { notice } => Cow::Borrowed(notice),
        RecipientActionState::Pending { .. } => {
            let noun = match action {
                RecipientAction::Acknowledge => "acknowledgement",
                RecipientAction::Defer => "defer reply",
                RecipientAction::ShareStatus => "status reply",
                RecipientAction::ShareRetro => "retrospective reply",
                RecipientAction::Reply | RecipientAction::Response => "response",
                RecipientAction::ContextSelect => "context selection",
                RecipientAction::ContextRemove => "context removal",
                RecipientAction::TeamSend => "team send",
                RecipientAction::TeamBoard => "team board",
                RecipientAction::TeamThread => "team thread",
                RecipientAction::TeamRetract => "team retract",
                RecipientAction::TeamStatus => "team status",
                RecipientAction::RevokeSenderConsent => "sender consent revocation",
            };
            let id = target_id(target);
            Cow::Owned(format!(
                "applying {noun} for {}…",
                sanitize_disclosable(id, id.len())
            ))
        }
    }
}

pub(crate) fn surface_action_notices<'a>(
    actions: &'a BTreeMap<(RecipientAction, RecipientActionTarget), RecipientActionState>,
    surface: &'a ThreadTarget,
    node: Option<&'a str>,
) -> impl Iterator<Item = Cow<'a, str>> + 'a {
    actions
        .iter()
        .filter(move |((_, target), _)| surface_matches(surface, node, target))
        .map(|((action, target), phase)| action_notice(*action, target, phase))
}

fn action_target_is_visible(state: &TuiState, target: &RecipientActionTarget) -> bool {
    state
        .thread_inspector
        .as_ref()
        .is_some_and(|surface| surface_matches(&surface.target, surface.node_id.as_deref(), target))
}

/// Story 19.25 `AC3` — drop the leave-out overlay of every item whose
/// retrospective card has left: no control for it still awaits the operator
/// with a stored preview.
pub(crate) fn prune_retro_left_out(
    state: &mut TuiState,
    conversation: &crate::domain::models::Conversation,
) {
    if state.retro_left_out.is_empty() {
        return;
    }
    let before = state.retro_left_out.len();
    state.retro_left_out.retain(|item_id, _| {
        conversation.peer_row_controls.values().any(|control| {
            control.stage == crate::domain::models::PeerRowStage::AwaitingOperator
                && control.retro.is_some()
                && control
                    .item_address
                    .as_ref()
                    .is_some_and(|address| address.item().as_str() == item_id)
        })
    });
    if state.retro_left_out.len() != before {
        state.needs_redraw = true;
    }
}

pub(crate) fn refresh_action_notices(state: &mut TuiState, visible_changed: bool) {
    if visible_changed && let Some(inspector) = state.thread_inspector.as_mut() {
        inspector.invalidate();
        inspector.scroll_to_end();
    }
    let mut message = String::new();
    for ((action, target), phase) in &state.recipient_actions {
        if action_target_is_visible(state, target) {
            continue;
        }
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(&action_notice(*action, target, phase));
    }
    let id = "recipient-actions";
    if message.is_empty() {
        state.feedback_blocks.remove(id);
    } else {
        crate::adapters::tui::views::show_view(
            &mut state.feedback_blocks,
            crate::adapters::tui::views::ViewKind::RecipientOutcome,
            id,
            FeedbackLevel::Info,
            message,
        );
    }
    state.needs_redraw = true;
}

fn target_id(target: &RecipientActionTarget) -> &str {
    match target {
        RecipientActionTarget::Item { item_id } => item_id,
        RecipientActionTarget::Node { node_id } => node_id,
        RecipientActionTarget::Request { request } => request,
    }
}

fn surface_matches(
    surface: &ThreadTarget,
    node: Option<&str>,
    target: &RecipientActionTarget,
) -> bool {
    match target {
        RecipientActionTarget::Item { item_id } => {
            matches!(surface, ThreadTarget::Item(address) if address.item().as_str() == item_id)
        }
        RecipientActionTarget::Node { node_id } => node == Some(node_id.as_str()),
        RecipientActionTarget::Request { .. } => false,
    }
}

const MAX_RECIPIENT_NOTICES: usize = 8;

fn acknowledge_surface_item(state: &mut TuiState, target: &RecipientActionTarget) {
    if let Some(inspector) = state.thread_inspector.as_mut()
        && surface_matches(&inspector.target, inspector.node_id.as_deref(), target)
    {
        inspector.item_state = Some("acknowledged");
        if let RecipientActionTarget::Item { item_id } = target {
            let prefix = format!("  {item_id} · received");
            if let Some(header) = inspector
                .content
                .lines()
                .find(|line| line.starts_with(&prefix))
                .map(str::to_owned)
            {
                inspector.content = inspector.content.replacen(
                    &header,
                    &header.replacen(" · received", " · acknowledged", 1),
                    1,
                );
            }
        }
        inspector.invalidate();
    }
}

pub(crate) fn show_notice(
    state: &mut TuiState,
    target: Option<&RecipientActionTarget>,
    message: String,
) {
    if let Some(inspector) = state.thread_inspector.as_mut()
        && target.is_none_or(|target| {
            surface_matches(&inspector.target, inspector.node_id.as_deref(), target)
        })
    {
        if inspector.results.len() >= MAX_RECIPIENT_NOTICES {
            inspector.results.remove(0);
        }
        inspector.results.push(message);
        inspector.invalidate();
        inspector.scroll_to_end();
    } else {
        crate::adapters::tui::views::show_view(
            &mut state.feedback_blocks,
            crate::adapters::tui::views::ViewKind::RecipientOutcome,
            "recipient-actions",
            FeedbackLevel::Info,
            message,
        );
    }
    state.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::daemon::protocol::{PeerDraftAction, ProtocolError};
    use crate::adapters::tui::state::ThreadInspectorState;
    use crate::adapters::tui::state::{PeerComposeAction, PinnedPeerCompose};

    #[test]
    fn status_share_and_decline_share_one_pending_key_and_notice() {
        let mut state = TuiState::new(80, 24);
        let share = ClientFrame::ShareRecipientStatus {
            item_id: "ri_status".to_owned(),
            decline: false,
        };
        let decline = ClientFrame::ShareRecipientStatus {
            item_id: "ri_status".to_owned(),
            decline: true,
        };
        assert_eq!(action_key(&share), action_key(&decline));
        assert_eq!(
            action_key(&share),
            Some((
                RecipientAction::ShareStatus,
                RecipientActionTarget::Item {
                    item_id: "ri_status".to_owned(),
                }
            ))
        );
        assert!(begin_action(&mut state, &share, false));
        assert!(!begin_action(&mut state, &decline, false));
        assert_eq!(state.recipient_actions.len(), 1);
        assert_eq!(
            state.feedback_blocks["recipient-actions"].message,
            "applying status reply for ri_status…"
        );
        state.input_buffer = "unfinished operator text".to_owned();
        abort_unsent_action(&mut state, share, "channel closed".to_owned());
        assert_eq!(state.input_buffer, "unfinished operator text");
        assert!(matches!(
            state.recipient_actions.values().next(),
            Some(RecipientActionState::Completed { notice }) if notice == "channel closed"
        ));
        assert!(begin_action(&mut state, &decline, false));
    }

    #[test]
    fn unrelated_result_does_not_release_composition_and_refusal_restores_its_pin() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = "line one\n  line two".to_owned();
        state.pinned_peer_compose = Some(PinnedPeerCompose::new(
            "node-b".to_owned(),
            PeerComposeAction::Edit,
        ));
        let frame = ClientFrame::ResolvePeerDraft {
            node: "node-b".to_owned(),
            action: PeerDraftAction::Edit {
                content: state.input_buffer.clone(),
            },
        };
        assert!(begin_action(&mut state, &frame, true));
        assert_eq!(pending_composition(&state), Some("line one\n  line two"));
        assert!(state.input_buffer.is_empty());
        assert!(state.pinned_peer_compose.is_none());
        apply_action_result(
            &mut state,
            RecipientAction::Response,
            RecipientActionTarget::Node {
                node_id: "node-a".to_owned(),
            },
            RecipientActionOutcome::Idempotent,
        );
        assert_eq!(pending_composition(&state), Some("line one\n  line two"));
        assert!(state.input_buffer.is_empty());
        apply_action_result(
            &mut state,
            RecipientAction::Response,
            RecipientActionTarget::Node {
                node_id: "node-b".to_owned(),
            },
            RecipientActionOutcome::Refused {
                error: ProtocolError::ReadOnly,
            },
        );
        assert_eq!(state.input_buffer, "line one\n  line two");
        assert_eq!(state.cursor_position, state.input_buffer.chars().count());
        assert_eq!(
            state.pinned_peer_compose.as_ref().unwrap().node_id,
            "node-b"
        );
        assert_eq!(
            state.pinned_peer_compose.as_ref().unwrap().action,
            PeerComposeAction::Edit
        );
        assert!(pending_composition(&state).is_none());
    }

    #[test]
    fn action_kind_is_part_of_pending_identity_and_duplicates_do_not_replace_text() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = "  /team reply ri-a first\n second".to_owned();
        let reply = ClientFrame::ReplyToRecipientItem {
            item_id: "ri-a".to_owned(),
            text: "first\n second".to_owned(),
            verdict: None,
        };
        assert!(begin_action(&mut state, &reply, true));
        state
            .input_buffer
            .push_str("must remain editable if refused before enqueue");
        assert!(!begin_action(&mut state, &reply, true));
        assert_eq!(
            state.input_buffer,
            "must remain editable if refused before enqueue"
        );
        apply_action_result(
            &mut state,
            RecipientAction::Acknowledge,
            RecipientActionTarget::Item {
                item_id: "ri-a".to_owned(),
            },
            RecipientActionOutcome::Idempotent,
        );
        assert_eq!(
            pending_composition(&state),
            Some("  /team reply ri-a first\n second")
        );
        state.input_buffer.clear();
        apply_action_result(
            &mut state,
            RecipientAction::Reply,
            RecipientActionTarget::Item {
                item_id: "ri-a".to_owned(),
            },
            RecipientActionOutcome::Refused {
                error: ProtocolError::ReadOnly,
            },
        );
        assert_eq!(state.input_buffer, "  /team reply ri-a first\n second");
        assert!(pending_composition(&state).is_none());
    }

    #[test]
    fn repeated_ack_consumes_its_command_instead_of_restoring_unsent_reply_text() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = "/team ack ri-a".to_owned();
        assert!(begin_action(
            &mut state,
            &ClientFrame::AcknowledgeRecipientItem {
                item_id: "ri-a".to_owned()
            },
            true,
        ));
        assert_eq!(pending_composition(&state), Some("/team ack ri-a"));
        apply_action_result(
            &mut state,
            RecipientAction::Acknowledge,
            RecipientActionTarget::Item {
                item_id: "ri-a".to_owned(),
            },
            RecipientActionOutcome::Idempotent,
        );
        assert!(
            state.input_buffer.is_empty(),
            "an acknowledged command must not reappear as unsent reply composition"
        );
        assert!(
            matches!(
                state.recipient_actions.get(&(
                    RecipientAction::Acknowledge,
                    RecipientActionTarget::Item {
                        item_id: "ri-a".to_owned()
                    },
                )),
                Some(RecipientActionState::Completed { .. })
            ),
            "K09: idempotent Ack terminates its pending marker"
        );
    }

    #[test]
    fn unrelated_ack_result_does_not_clear_the_pending_marker() {
        let mut state = TuiState::new(80, 24);
        let first = ClientFrame::AcknowledgeRecipientItem {
            item_id: "ri-first".to_owned(),
        };
        assert!(begin_action(&mut state, &first, false));
        apply_action_result(
            &mut state,
            RecipientAction::Acknowledge,
            RecipientActionTarget::Item {
                item_id: "ri-other".to_owned(),
            },
            RecipientActionOutcome::Idempotent,
        );
        assert!(
            matches!(
                state.recipient_actions.get(&(
                    RecipientAction::Acknowledge,
                    RecipientActionTarget::Item {
                        item_id: "ri-first".to_owned()
                    },
                )),
                Some(RecipientActionState::Pending { .. })
            ),
            "K09: an unrelated Ack result cannot clear the original pending marker"
        );
    }

    #[test]
    fn ack_result_never_selects_recipient_context() {
        let mut state = TuiState::new(80, 24);
        apply_action_result(
            &mut state,
            RecipientAction::Acknowledge,
            RecipientActionTarget::Item {
                item_id: "ri-ack-context-control".to_owned(),
            },
            RecipientActionOutcome::Applied { reply: None },
        );
        assert!(
            state.recipient_context_ids.is_empty(),
            "K09: acknowledging an item does not implicitly select peer context"
        );
    }

    #[test]
    fn reader_notices_keep_only_the_most_recent_bounded_history() {
        let mut state = TuiState::new(80, 24);
        let item = crate::domain::models::ItemAddress::from_a2a_ingress(
            crate::domain::models::PeerId::from_public_key(&[7; 32]).unwrap(),
            crate::domain::models::ItemId::from_replay("notice-item"),
        );
        crate::adapters::tui::presentation::reducer::open_thread(
            &mut state,
            ThreadInspectorState::new(
                ThreadTarget::Item(item),
                crate::adapters::tui::state::ThreadAccess::LocalOnly,
                crate::domain::models::SemanticMessageType::Unknown,
            ),
            crate::adapters::tui::thread_inspector::title(
                crate::domain::models::SemanticMessageType::Unknown,
            ),
        );
        for index in 0..MAX_RECIPIENT_NOTICES + 1 {
            show_notice(&mut state, None, format!("notice {index}"));
        }
        let results = &state.thread_inspector.as_ref().unwrap().results;
        assert_eq!(results.len(), MAX_RECIPIENT_NOTICES);
        assert_eq!(results.first().unwrap(), "notice 1");
        assert_eq!(results.last().unwrap(), "notice 8");
    }

    fn retro_frame(item_id: &str, decline: bool) -> ClientFrame {
        ClientFrame::ShareRecipientRetro {
            item_id: item_id.to_owned(),
            leave_out: if decline {
                Vec::new()
            } else {
                vec![crate::domain::models::RetroField::ReplyTimes]
            },
            decline,
        }
    }

    #[test]
    fn story19_25_retro_share_and_decline_share_one_pending_key_and_notice() {
        let mut state = TuiState::new(80, 24);
        let share = retro_frame("ri_retro", false);
        let decline = retro_frame("ri_retro", true);
        let key = (
            RecipientAction::ShareRetro,
            RecipientActionTarget::Item {
                item_id: "ri_retro".to_owned(),
            },
        );
        assert_eq!(action_key(&share), Some(key.clone()));
        assert_eq!(action_key(&decline), Some(key.clone()));
        assert!(begin_action(&mut state, &share, false));
        assert!(!begin_action(&mut state, &decline, false));
        assert_eq!(state.recipient_actions.len(), 1);
        assert_eq!(
            state.feedback_blocks["recipient-actions"].message,
            "applying retrospective reply for ri_retro…"
        );
        state.input_buffer = "unfinished operator text".to_owned();
        abort_unsent_action(&mut state, decline.clone(), "channel closed".to_owned());
        assert_eq!(state.input_buffer, "unfinished operator text");
        assert!(matches!(
            state.recipient_actions.get(&key),
            Some(RecipientActionState::Completed { notice }) if notice == "channel closed"
        ));
        assert!(begin_action(&mut state, &decline, false));
    }

    #[test]
    fn story19_25_retro_overlay_clears_only_when_its_act_settles() {
        use crate::domain::models::{RetroField, RetroFieldSet};
        let left_out: RetroFieldSet = [RetroField::ReplyTimes].into_iter().collect();
        let target = |item_id: &str| RecipientActionTarget::Item {
            item_id: item_id.to_owned(),
        };
        for (outcome, cleared) in [
            (RecipientActionOutcome::Applied { reply: None }, true),
            (RecipientActionOutcome::Idempotent, true),
            (
                RecipientActionOutcome::Refused {
                    error: ProtocolError::ReadOnly,
                },
                false,
            ),
        ] {
            let mut state = TuiState::new(80, 24);
            state
                .retro_left_out
                .insert("ri_retro".to_owned(), left_out.clone());
            state
                .retro_left_out
                .insert("ri_other".to_owned(), left_out.clone());
            assert!(begin_action(
                &mut state,
                &retro_frame("ri_retro", false),
                false
            ));
            // Another act's result on the same item, or this act's on
            // another item, never touches the overlay.
            apply_action_result(
                &mut state,
                RecipientAction::ShareStatus,
                target("ri_retro"),
                RecipientActionOutcome::Idempotent,
            );
            apply_action_result(
                &mut state,
                RecipientAction::Defer,
                target("ri_retro"),
                RecipientActionOutcome::Applied { reply: None },
            );
            assert_eq!(state.retro_left_out.get("ri_retro"), Some(&left_out));
            apply_action_result(
                &mut state,
                RecipientAction::ShareRetro,
                target("ri_retro"),
                outcome,
            );
            assert_eq!(
                state.retro_left_out.contains_key("ri_retro"),
                !cleared,
                "a refusal keeps the redactions for a retry"
            );
            assert_eq!(state.retro_left_out.get("ri_other"), Some(&left_out));
        }
    }

    #[test]
    fn story19_25_retro_overlay_is_pruned_when_the_card_leaves() {
        use crate::domain::models::{
            Conversation, ItemAddress, ItemId, PeerId, PeerRowControl, PeerRowStage,
            RetroContribution, RetroField, RetroFieldSet, RetroPreview, SemanticMessageType,
        };
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[73; 32]).unwrap(),
            ItemId::from_replay("ri_retro"),
        );
        let control = |stage, retro: bool| PeerRowControl {
            node_id: "retro-node".to_owned(),
            item_address: Some(address.clone()),
            stories: None,
            status: None,
            retro: retro.then(|| RetroPreview {
                contribution: RetroContribution::nothing(),
                nothing_in_window: true,
            }),
            selected_for: None,
            message_type: SemanticMessageType::RetroRequest,
            sender_label: "sender".to_owned(),
            stage,
            node_resolvable: false,
        };
        let conversation_with = |control: PeerRowControl| Conversation {
            peer_row_controls: [("retro-row".to_owned(), control)].into_iter().collect(),
            ..Default::default()
        };
        let left_out: RetroFieldSet = [RetroField::Received].into_iter().collect();
        let mut state = TuiState::new(80, 24);
        // A restored (unresolvable) card awaiting the operator keeps it.
        let waiting = conversation_with(control(PeerRowStage::AwaitingOperator, true));
        state
            .retro_left_out
            .insert("ri_retro".to_owned(), left_out.clone());
        state.needs_redraw = false;
        prune_retro_left_out(&mut state, &waiting);
        assert_eq!(state.retro_left_out.get("ri_retro"), Some(&left_out));
        assert!(!state.needs_redraw);
        for gone in [
            conversation_with(control(PeerRowStage::Settled, true)),
            conversation_with(control(PeerRowStage::Drafting, true)),
            conversation_with(control(PeerRowStage::AwaitingOperator, false)),
            Conversation::default(),
        ] {
            state
                .retro_left_out
                .insert("ri_retro".to_owned(), left_out.clone());
            state.needs_redraw = false;
            prune_retro_left_out(&mut state, &gone);
            assert!(state.retro_left_out.is_empty());
            assert!(state.needs_redraw);
        }
    }
}
