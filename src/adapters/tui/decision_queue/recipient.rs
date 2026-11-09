//! Attached peer-response decisions projected from the host's authoritative rows.

use super::{
    Arrival, DecisionDraft, DecisionKey, DecisionKind, DecisionSubject, NO_LONGER_PENDING,
    Responder, raise, withdraw_key,
};
use crate::adapters::tui::{presentation::text_field::TextField, state::TuiState};
use crate::domain::models::{
    Conversation, ItemAddress, PeerRowStage, RetroPreview, SelectionSummary, StatusPreview,
    StorySummary, semantic_message_type_metadata,
};

/// A response can be written immediately, or approved after an agent drafted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipientDraftStage {
    AwaitingOperator,
    Ready(String),
}

/// Which operation the frame-owned field will submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientComposeMode {
    Edit,
    WriteOwn,
}

/// The authoritative response target and its presentation, independent of the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientDraftPayload {
    pub node: String,
    pub sender_label: String,
    /// Original bytes for Copy; never painted.
    pub sender_source: String,
    pub canonical_type: &'static str,
    pub stage: RecipientDraftStage,
    pub item_address: Option<ItemAddress>,
    pub stories: Option<StorySummary>,
    pub status: Option<StatusPreview>,
    pub selected_for: Option<SelectionSummary>,
    pub retro: Option<RetroPreview>,
    pub compose: Option<(RecipientComposeMode, TextField)>,
}

/// Synchronize this conversation's live rows without disturbing another conversation.
/// A mirror never acquires an actionable response. Existing fields survive row updates.
pub fn sync_recipient_drafts(state: &mut TuiState, conversation: &Conversation, read_only: bool) {
    let mut seen = std::collections::HashSet::new();
    if !read_only {
        for message in &conversation.messages {
            let Some(control) = conversation.peer_row_controls.get(&message.id) else {
                continue;
            };
            if !control.node_resolvable {
                continue;
            }
            let stage = match control.stage {
                PeerRowStage::AwaitingOperator => RecipientDraftStage::AwaitingOperator,
                PeerRowStage::Ready => RecipientDraftStage::Ready(
                    message
                        .content
                        .strip_prefix(
                            crate::adapters::daemon::response_modes::DRAFT_APPROVAL_PREFIX,
                        )
                        .unwrap_or(&message.content)
                        .to_owned(),
                ),
                PeerRowStage::Drafting | PeerRowStage::Settled => continue,
            };
            if !seen.insert(control.node_id.clone()) {
                continue;
            }
            let key = DecisionKey {
                kind: DecisionKind::RecipientDraftApproval,
                source: control.node_id.clone(),
                conversation_id: Some(conversation.id.clone()),
            };
            let label: String =
                crate::domain::services::peer_text::sanitize_display_line(&control.sender_label)
                    .chars()
                    .take(512)
                    .collect();
            let mut payload = RecipientDraftPayload {
                node: control.node_id.clone(),
                sender_label: label,
                sender_source: control.sender_label.clone(),
                canonical_type: semantic_message_type_metadata(control.message_type).operator_noun,
                stage,
                item_address: control.item_address.clone(),
                stories: control.stories.clone(),
                status: control.status.clone(),
                selected_for: control.selected_for.clone(),
                retro: control.retro.clone(),
                compose: None,
            };
            if let Some(item) = state.decisions.item_mut(&key) {
                if let DecisionSubject::RecipientDraft(old) = &mut item.subject
                    && old.stage == payload.stage
                    && old.item_address == payload.item_address
                {
                    payload.compose = old.compose.take();
                }
                item.summary.clone_from(&payload.sender_label);
                item.subject = DecisionSubject::RecipientDraft(Box::new(payload));
                item.validity = super::Validity::Live;
                *state.decisions.body_layout.get_mut() = None;
                state.needs_redraw = true;
            } else {
                let draft = DecisionDraft::new(DecisionKind::RecipientDraftApproval, "")
                    .source(control.node_id.clone())
                    .conversation(conversation.id.clone())
                    .summary(payload.sender_label.clone())
                    .responder(Responder::Attached)
                    .subject(DecisionSubject::RecipientDraft(Box::new(payload)));
                raise(state, draft, Arrival::Automatic);
            }
        }
    }
    let gone: Vec<_> = state
        .decisions
        .items()
        .filter(|item| {
            item.kind == DecisionKind::RecipientDraftApproval
                && !item.is_stale()
                && item.conversation_id.as_deref() == Some(conversation.id.as_str())
                && !seen.contains(&item.source)
        })
        .map(super::DecisionItem::key)
        .collect();
    for key in gone {
        withdraw_key(state, &key, NO_LONGER_PENDING);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ChatMessage, ItemId, PeerId, PeerRowControl, SemanticMessageType};

    fn conversation(id: &str, alias: &str) -> Conversation {
        let peer = PeerId::from_public_key(&[9; 32]).unwrap();
        let control = PeerRowControl {
            node_id: "node-a".to_owned(),
            item_address: Some(ItemAddress::from_a2a_ingress(
                peer,
                ItemId::from_replay("ri-a"),
            )),
            stories: None,
            status: None,
            retro: None,
            selected_for: None,
            message_type: SemanticMessageType::Consultation,
            sender_label: alias.to_owned(),
            stage: PeerRowStage::Ready,
            node_resolvable: true,
        };
        Conversation {
            id: id.to_owned(),
            messages: vec![ChatMessage {
                id: "row".to_owned(),
                content: "draft".to_owned(),
                ..Default::default()
            }],
            peer_row_controls: [("row".to_owned(), control)].into_iter().collect(),
            ..Default::default()
        }
    }

    #[test]
    fn story21_6_attached_alias_and_principal_are_safe_before_cells() {
        let conversation = conversation("origin", "Nina\x1b[31m red\u{202e}\nforged");
        let principal = match conversation.peer_row_controls["row"]
            .item_address
            .as_ref()
            .unwrap()
            .principal()
        {
            crate::domain::models::ItemPrincipal::A2aPseudonym(peer) => peer.as_str().to_owned(),
            _ => unreachable!(),
        };
        let mut state = TuiState::new(80, 24);
        sync_recipient_drafts(&mut state, &conversation, false);
        let item = state.decisions.items().next().unwrap();
        let lines =
            crate::adapters::tui::widgets::decision_frame::body_lines_for_test(item, &state, 76);
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Nina red forged"), "{text:?}");
        assert!(text.contains(&principal), "{text:?}");
        for span in lines.iter().flat_map(|line| &line.spans) {
            assert!(!span.content.contains('\x1b'));
            assert!(!span.content.contains('\u{202e}'));
            assert!(!span.content.contains('\n'));
        }
    }

    #[test]
    fn story21_6_attached_projection_scopes_updates_and_excludes_mirrors() {
        let mut state = TuiState::new(80, 24);
        let mut a = conversation("a", "A");
        let b = conversation("b", "B");
        sync_recipient_drafts(&mut state, &a, false);
        sync_recipient_drafts(&mut state, &b, false);
        assert_eq!(
            state
                .decisions
                .items()
                .filter(|item| !item.is_stale())
                .count(),
            2
        );
        a.peer_row_controls.get_mut("row").unwrap().stage = PeerRowStage::Settled;
        sync_recipient_drafts(&mut state, &a, false);
        assert_eq!(
            state
                .decisions
                .items()
                .filter(|item| !item.is_stale())
                .count(),
            1
        );
        let mut mirror = TuiState::new(80, 24);
        sync_recipient_drafts(&mut mirror, &b, true);
        assert_eq!(mirror.decisions.items().count(), 0);
    }

    #[test]
    fn story21_6_attached_copy_keeps_original_bytes_and_enforces_exact_limit() {
        use crate::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
        use crate::adapters::tui::widgets::decision_frame::{CopySource, copy_source};
        let mut conversation = conversation("origin", "alias\x1b[31m");
        conversation.messages[0].content.clear();
        let mut state = TuiState::new(80, 24);
        sync_recipient_drafts(&mut state, &conversation, false);
        let CopySource::Text(base) = copy_source(state.decisions.items().next().unwrap(), &state)
        else {
            panic!("small source should copy");
        };
        assert!(base.contains("alias\x1b[31m"));
        let length = COPY_MAX_BYTES - base.len();
        conversation.messages[0].content = "x".repeat(length);
        sync_recipient_drafts(&mut state, &conversation, false);
        let CopySource::Text(exact) = copy_source(state.decisions.items().next().unwrap(), &state)
        else {
            panic!("exact limit should copy");
        };
        assert_eq!(exact.len(), COPY_MAX_BYTES);
        assert!(exact.ends_with(&"x".repeat(length)));
        conversation.messages[0].content.push('x');
        sync_recipient_drafts(&mut state, &conversation, false);
        assert_eq!(
            copy_source(state.decisions.items().next().unwrap(), &state),
            CopySource::TooLarge
        );
    }

    #[test]
    fn story21_6_recipient_preview_marks_both_prefix_limits() {
        for (draft, marked) in [
            ("x".repeat(512), false),
            ("x".repeat(513), true),
            ("\u{202e}".repeat(4096), false),
            ("\u{202e}".repeat(4097), true),
        ] {
            let mut conversation = conversation("origin", "sender");
            conversation.messages[0].content = draft;
            let mut state = TuiState::new(80, 24);
            sync_recipient_drafts(&mut state, &conversation, false);
            let lines = crate::adapters::tui::widgets::decision_frame::body_lines_for_test(
                state.decisions.items().next().unwrap(),
                &state,
                10000,
            );
            let preview = lines
                .iter()
                .map(ToString::to_string)
                .find(|line| line.starts_with("Peer draft: "))
                .unwrap();
            assert_eq!(preview.ends_with('…'), marked, "{preview:?}");
        }
    }
}
