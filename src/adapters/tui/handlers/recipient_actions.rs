use crate::adapters::daemon::protocol::{
    ClientFrame, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
};
use crate::adapters::tui::state::{RecipientActionState, ThreadTarget, TuiState};
use crate::domain::models::FeedbackLevel;
use crate::domain::services::transparency::{MAX_PEER_ID_BYTES, sanitize_disclosable};
use std::borrow::Cow;

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
        })
}

/// Whether a reply sent from a thread's own field still awaits its result: its text is
/// recoverable only while the session lives, so quitting must ask (8B-03).
pub(crate) fn pending_field_reply(state: &TuiState) -> bool {
    state
        .recipient_actions
        .values()
        .any(|action| matches!(action, RecipientActionState::Pending { reply: Some(_), .. }))
}

/// The outcome-log key of an act's target (`None`: a `/team` request names no thread).
fn outcome_key(target: &RecipientActionTarget) -> Option<String> {
    match target {
        RecipientActionTarget::Item { item_id } => Some(item_id.clone()),
        RecipientActionTarget::Node { node_id } => Some(format!("node:{node_id}")),
        RecipientActionTarget::Request { .. } => None,
    }
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
    let visible = action_target_is_visible(state, &key.1);
    // A reply sent from the thread's own field keeps its text until the daemon settles it.
    let reply = match frame {
        ClientFrame::ReplyToRecipientItem { text, .. } if !composed && !text.is_empty() => {
            Some(text.clone())
        }
        _ => None,
    };
    let composed_input = composed.then(|| {
        state.cursor_position = 0;
        std::mem::take(&mut state.input_buffer)
    });
    state.recipient_actions.insert(
        key,
        RecipientActionState::Pending {
            composed_input,
            reply,
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
                // A `/team` request key never names a thread.
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
    let (submitted, reply) = pending
        .map(|phase| match phase {
            RecipientActionState::Pending {
                composed_input,
                reply,
            } => (composed_input, reply),
        })
        .unwrap_or((None, None));
    let thread_key = outcome_key(target);
    let id = sanitize_disclosable(target_id(target), MAX_PEER_ID_BYTES);
    let message = match outcome {
        RecipientActionOutcome::Applied { reply: Some(reply) } => {
            if let Some(thread) = &thread_key {
                state.thread_outcomes.take_retained(thread);
            }
            format!(
                "reply recorded for {} as {} — the asker sees it when they read the thread",
                sanitize_disclosable(&reply.item_id, MAX_PEER_ID_BYTES),
                sanitize_disclosable(&reply.turn_id, MAX_PEER_ID_BYTES),
            )
        }
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
                    "no new action applied — acknowledgement already recorded for {id} (receipt only)"
                )
            } else if let Some(text) = submitted {
                state.cursor_position = text.chars().count();
                state.input_buffer = text;
                "response already settled — your text was not sent".to_owned()
            } else if let (Some(text), Some(thread)) = (reply, &thread_key) {
                state.thread_outcomes.retain_reply(thread, text);
                "response already settled — your text was not sent".to_owned()
            } else {
                "response already settled — no action applied".to_owned()
            }
        }
        RecipientActionOutcome::Refused { error } => {
            let step = if let Some(text) = submitted {
                state.cursor_position = text.chars().count();
                state.input_buffer = text;
                " Your text is back in the input; change it and send it again."
            } else if let (Some(text), Some(thread)) = (reply, &thread_key) {
                state.thread_outcomes.retain_reply(thread, text);
                " Your reply is kept; open the thread and send it again."
            } else {
                ""
            };
            let error = error.to_string();
            format!("[daemon error] {}.{step}", error.trim_end_matches('.'))
        }
    };
    show_notice(state, Some(target), message);
}

pub(crate) fn apply_context_state(
    state: &mut TuiState,
    ids: Vec<String>,
    inventory: String,
    show_inventory: bool,
) {
    state.recipient_context_ids = ids.into_iter().collect();
    if let Some(inspector) = state.thread_inspector.as_mut()
        && let Some(address) = inspector.item_address()
    {
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
            crate::adapters::tui::views::ViewKind::RecipientContext,
            "recipient-context",
            FeedbackLevel::Info,
            inventory,
        );
    }
    state.needs_redraw = true;
}

pub(crate) fn abort_unsent_action(state: &mut TuiState, frame: ClientFrame, message: String) {
    let Some(key) = action_key(&frame) else {
        return;
    };
    let thread_key = outcome_key(&key.1);
    match state.recipient_actions.remove(&key) {
        Some(RecipientActionState::Pending {
            composed_input: Some(text),
            ..
        }) => {
            state.cursor_position = text.chars().count();
            state.input_buffer = text;
        }
        Some(RecipientActionState::Pending {
            reply: Some(text), ..
        }) => {
            if let Some(thread) = &thread_key {
                state.thread_outcomes.retain_reply(thread, text);
            }
        }
        _ => {}
    }
    show_notice(state, Some(&key.1), message);
}

/// The words of an act still in flight (the thread shows them until the outcome replaces them).
fn pending_notice(action: RecipientAction, target: &RecipientActionTarget) -> Cow<'static, str> {
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

/// The in-flight notices of acts on `surface`'s target.
pub(crate) fn surface_action_notices<'a>(
    actions: &'a std::collections::BTreeMap<
        (RecipientAction, RecipientActionTarget),
        RecipientActionState,
    >,
    surface: &'a ThreadTarget,
    node: Option<&'a str>,
) -> impl Iterator<Item = Cow<'a, str>> + 'a {
    actions
        .iter()
        .filter(move |((_, target), _)| surface_matches(surface, node, target))
        .map(|((action, target), _)| pending_notice(*action, target))
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

/// An in-flight act changed: the open thread repaints its notices (a visible change also
/// scrolls to them). Outcomes are never transcript rows.
pub(crate) fn refresh_action_notices(state: &mut TuiState, visible_changed: bool) {
    if visible_changed && let Some(inspector) = state.thread_inspector.as_mut() {
        inspector.invalidate();
        inspector.scroll_to_end();
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
        RecipientActionTarget::Node { node_id } => {
            matches!(surface, ThreadTarget::Node(open) if open == node_id)
                || (matches!(surface, ThreadTarget::Item(_)) && node == Some(node_id.as_str()))
        }
        RecipientActionTarget::Request { .. } => false,
    }
}

fn acknowledge_surface_item(state: &mut TuiState, target: &RecipientActionTarget) {
    if let Some(inspector) = state.thread_inspector.as_mut()
        && surface_matches(&inspector.target, inspector.node_id.as_deref(), target)
    {
        inspector.item_state = Some("acknowledged");
        if let RecipientActionTarget::Item { item_id } = target {
            let prefix = format!("  {item_id} · received");
            let acknowledged = |text: &str| {
                text.lines()
                    .find(|line| line.starts_with(&prefix))
                    .map(|header| {
                        text.replacen(
                            header,
                            &header.replacen(" · received", " · acknowledged", 1),
                            1,
                        )
                    })
            };
            if let Some(text) = acknowledged(&inspector.content) {
                inspector.content = text;
            }
            for segment in &mut inspector.segments {
                if let crate::adapters::tui::thread_body::ThreadSegment::Text(text) = segment
                    && let Some(updated) = acknowledged(text)
                {
                    *text = updated;
                }
            }
        }
        inspector.invalidate();
    }
}

/// One outcome line of an item- or node-targeted act (Story 21.8 A13): it joins the target's
/// session outcome log; the thread of that target paints it, and when that thread is not the
/// window on top the line is also posted once as an ambient Info notice (S-LOG archives it).
/// `target: None` is the open thread's own target.
pub(crate) fn show_notice(
    state: &mut TuiState,
    target: Option<&RecipientActionTarget>,
    message: String,
) {
    use crate::adapters::tui::notifications::{self, NoticeSource};
    let open = state
        .thread_inspector
        .as_ref()
        .map(|inspector| inspector.outcome_key());
    let key = match target {
        Some(target) => outcome_key(target),
        None => open.clone(),
    };
    let on_top = state.modal_stack.top().is_some_and(|frame| {
        frame.surface == crate::adapters::tui::presentation::Surface::ThreadInspector
    }) && key.is_some()
        && open == key;
    if let Some(key) = &key {
        state.thread_outcomes.push(key, message.clone());
        if let Some(inspector) = state.thread_inspector.as_mut()
            && &inspector.outcome_key() == key
        {
            inspector.invalidate();
            inspector.scroll_to_end();
        }
    }
    if !on_top {
        notifications::info(state, NoticeSource::Team, message);
    }
    state.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::daemon::protocol::{PeerDraftAction, ProtocolError};
    use crate::adapters::tui::state::ThreadInspectorState;

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
            pending_notice(RecipientAction::ShareStatus, &action_key(&share).unwrap().1),
            "applying status reply for ri_status…"
        );
        assert!(!state.feedback_blocks.contains_key("recipient-actions"));
        state.input_buffer = "unfinished operator text".to_owned();
        abort_unsent_action(&mut state, share, "channel closed".to_owned());
        assert_eq!(state.input_buffer, "unfinished operator text");
        assert!(state.recipient_actions.is_empty());
        assert_eq!(
            state.thread_outcomes.lines("ri_status").collect::<Vec<_>>(),
            ["channel closed"]
        );
        assert!(begin_action(&mut state, &decline, false));
    }

    #[test]
    fn unrelated_result_does_not_release_composition_and_refusal_restores_text() {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = "line one\n  line two".to_owned();
        let frame = ClientFrame::ResolvePeerDraft {
            node: "node-b".to_owned(),
            action: PeerDraftAction::Edit {
                content: state.input_buffer.clone(),
            },
        };
        assert!(begin_action(&mut state, &frame, true));
        assert_eq!(pending_composition(&state), Some("line one\n  line two"));
        assert!(state.input_buffer.is_empty());
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
        assert!(pending_composition(&state).is_none());
        let notice = state
            .notifications
            .records()
            .last()
            .expect("the refusal is recorded")
            .message
            .clone();
        assert_eq!(
            notice,
            "[daemon error] this attachment is read-only; another client already has write access. Your text is back in the input; change it and send it again."
        );
    }

    #[test]
    fn a_refusal_names_the_daemon_reason_then_the_step_for_what_was_kept() {
        let reason =
            "[daemon error] this attachment is read-only; another client already has write access.";
        let item = || RecipientActionTarget::Item {
            item_id: "ri-a".to_owned(),
        };
        let refused = || RecipientActionOutcome::Refused {
            error: ProtocolError::ReadOnly,
        };

        let mut state = TuiState::new(80, 24);
        let reply = ClientFrame::ReplyToRecipientItem {
            item_id: "ri-a".to_owned(),
            text: "my reply".to_owned(),
            verdict: None,
        };
        assert!(begin_action(&mut state, &reply, false));
        apply_action_result(&mut state, RecipientAction::Reply, item(), refused());
        assert_eq!(
            state.thread_outcomes.lines("ri-a").collect::<Vec<_>>(),
            [format!(
                "{reason} Your reply is kept; open the thread and send it again."
            )]
        );

        let mut state = TuiState::new(80, 24);
        apply_action_result(&mut state, RecipientAction::Reply, item(), refused());
        assert_eq!(
            state.thread_outcomes.lines("ri-a").collect::<Vec<_>>(),
            [reason]
        );
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
            !state.recipient_actions.contains_key(&(
                RecipientAction::Acknowledge,
                RecipientActionTarget::Item {
                    item_id: "ri-a".to_owned()
                },
            )),
            "K09: idempotent Ack terminates its pending marker"
        );
        assert!(
            state
                .thread_outcomes
                .lines("ri-a")
                .any(|line| line.contains("no new action applied"))
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
        let target = RecipientActionTarget::Item {
            item_id: "notice-item".into(),
        };
        for index in 0..17 {
            show_notice(&mut state, Some(&target), format!("notice {index}"));
        }
        let results = state
            .thread_outcomes
            .lines("notice-item")
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 16);
        assert_eq!(results.first().unwrap(), &"notice 1");
        assert_eq!(results.last().unwrap(), &"notice 16");
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
            pending_notice(RecipientAction::ShareRetro, &key.1),
            "applying retrospective reply for ri_retro…"
        );
        assert!(!state.feedback_blocks.contains_key("recipient-actions"));
        state.input_buffer = "unfinished operator text".to_owned();
        abort_unsent_action(&mut state, decline.clone(), "channel closed".to_owned());
        assert_eq!(state.input_buffer, "unfinished operator text");
        assert!(!state.recipient_actions.contains_key(&key));
        assert_eq!(
            state.thread_outcomes.lines("ri_retro").collect::<Vec<_>>(),
            ["channel closed"]
        );
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
