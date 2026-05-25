//! Pure domain service for conversation compaction.
//! No adapter/infra imports — hexagonal compliance.

use crate::domain::models::conversation::{ChatMessage, CompactionState, Conversation};
use crate::domain::models::{Message, MessageRole};

/// System prompt for the compaction LLM call.
pub const COMPACTION_SYSTEM_PROMPT: &str = r#"You are compacting a software-engineering assistant conversation so it fits within the model's context window. Produce a dense, factual summary of the conversation below. The summary MUST preserve, in this order: (1) key decisions made and the reasoning behind them; (2) every file created or modified, with its current state and what was done to it; (3) the current task and its progress — what is done, what remains, and any blockers; (4) important recent exchanges, near-verbatim, so the assistant can continue seamlessly. Do not include pleasantries or meta-commentary. Output only the summary text."#;

/// Returns the id of the last User ChatMessage if there are at least 2 turns of history.
pub fn first_kept_message_id(conversation: &Conversation) -> Option<String> {
    let user_msgs: Vec<&ChatMessage> = conversation
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .collect();
    if user_msgs.len() >= 2 {
        user_msgs.last().map(|m| m.id.clone())
    } else {
        None
    }
}

/// Flatten messages before the boundary into a single string for the compaction prompt.
pub fn build_compaction_prompt_input(
    conversation: &Conversation,
    first_kept_message_id: &str,
    context_window: u32,
) -> String {
    // Find boundary index
    let boundary = conversation
        .messages
        .iter()
        .position(|m| m.id == first_kept_message_id)
        .unwrap_or(conversation.messages.len());

    let mut parts = Vec::new();

    // Prepend existing summary if re-compacting
    if let Some(ref cs) = conversation.compaction {
        parts.push(format!("[Earlier summary]: {}\n\n", cs.summary));
    }

    // Flatten messages before boundary
    for msg in &conversation.messages[..boundary] {
        let label = match msg.role {
            MessageRole::User => "[User]",
            MessageRole::Assistant => "[Assistant]",
            MessageRole::System => "[System]",
        };
        parts.push(format!("{}: {}\n\n", label, msg.content));
    }

    let mut result = parts.join("");

    // Budget guard: inline chars/4 estimate, drop oldest entries until it fits
    let budget = (context_window as usize * 7) / 10;
    while estimate_tokens_inline(&result) > budget && !parts.is_empty() {
        // Drop the oldest flattened entry (skip the earlier-summary prefix if present)
        let skip = if conversation.compaction.is_some() {
            1
        } else {
            0
        };
        if parts.len() > skip {
            parts.remove(skip);
            result = parts.join("");
        } else {
            break;
        }
    }

    result
}

/// Reshape messages for the active LLM context when compaction is present.
pub fn shape_compacted_messages(conversation: &Conversation, messages: &mut Vec<Message>) {
    let Some(ref cs) = conversation.compaction else {
        return;
    };

    // Find the first User message whose content matches the kept message's content
    let kept_content = conversation
        .messages
        .iter()
        .find(|m| m.id == cs.first_kept_message_id)
        .map(|m| m.content.as_str());

    let Some(kept_content) = kept_content else {
        tracing::warn!(
            "compaction boundary message {} not found in conversation.messages",
            cs.first_kept_message_id
        );
        return;
    };

    let boundary_idx = messages
        .iter()
        .position(|m| m.role == MessageRole::User && m.content == kept_content);

    let Some(boundary_idx) = boundary_idx else {
        tracing::warn!(
            "compaction boundary content not found in built API messages (kept id: {})",
            cs.first_kept_message_id
        );
        return;
    };

    // Drain everything before the boundary
    messages.drain(..boundary_idx);

    // Set context_prefix on the first remaining User message
    if let Some(first_user) = messages.iter_mut().find(|m| m.role == MessageRole::User) {
        let prefix = format!(
            "<conversation-summary>\n{}\n</conversation-summary>",
            cs.summary
        );
        first_user.context_prefix = Some(compose_context_prefix(
            first_user.context_prefix.take(),
            prefix,
        ));
    }
}

/// Compose a context prefix, joining with `\n\n` when both parts exist.
pub fn compose_context_prefix(existing: Option<String>, addition: String) -> String {
    match existing {
        Some(prev) => format!("{}\n\n{}", prev, addition),
        None => addition,
    }
}

/// Inline token estimate for budget guarding (chars/4, no adapter dependency).
fn estimate_tokens_inline(text: &str) -> usize {
    text.chars().count() / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::MessageRole;
    use crate::domain::models::conversation::{ChatMessage, Conversation};

    fn make_msg(id: &str, role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            id: id.to_string(),
            role,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        }
    }

    #[test]
    fn test_first_kept_message_id_less_than_two_turns() {
        let conv = Conversation {
            messages: vec![make_msg("m1", MessageRole::User, "hi")],
            ..Default::default()
        };
        assert_eq!(first_kept_message_id(&conv), None);
    }

    #[test]
    fn test_first_kept_message_id_two_turns() {
        let conv = Conversation {
            messages: vec![
                make_msg("m1", MessageRole::User, "hi"),
                make_msg("m2", MessageRole::Assistant, "hello"),
                make_msg("m3", MessageRole::User, "bye"),
            ],
            ..Default::default()
        };
        assert_eq!(first_kept_message_id(&conv), Some("m3".to_string()));
    }

    #[test]
    fn test_build_compaction_prompt_input_flattening() {
        let conv = Conversation {
            messages: vec![
                make_msg("m1", MessageRole::User, "Q1"),
                make_msg("m2", MessageRole::Assistant, "A1"),
                make_msg("m3", MessageRole::User, "Q2"),
            ],
            ..Default::default()
        };
        let input = build_compaction_prompt_input(&conv, "m3", 1_000_000);
        assert!(input.contains("[User]: Q1"));
        assert!(input.contains("[Assistant]: A1"));
        assert!(!input.contains("Q2")); // boundary and after are excluded
    }

    #[test]
    fn test_build_compaction_prompt_input_recompaction_prepend() {
        let conv = Conversation {
            messages: vec![
                make_msg("m1", MessageRole::User, "Q1"),
                make_msg("m2", MessageRole::Assistant, "A1"),
                make_msg("m3", MessageRole::User, "Q2"),
            ],
            compaction: Some(CompactionState {
                summary: "Prior summary".to_string(),
                first_kept_message_id: "m3".to_string(),
                compacted_at: 0,
                pre_compaction_tokens: 100,
            }),
            ..Default::default()
        };
        let input = build_compaction_prompt_input(&conv, "m3", 1_000_000);
        assert!(input.contains("[Earlier summary]: Prior summary"));
        assert!(input.contains("[User]: Q1"));
    }

    #[test]
    fn test_shape_compacted_messages_slices_and_prefixes() {
        let conv = Conversation {
            messages: vec![
                make_msg("m1", MessageRole::User, "Q1"),
                make_msg("m2", MessageRole::Assistant, "A1"),
                make_msg("m3", MessageRole::User, "Q2"),
            ],
            compaction: Some(CompactionState {
                summary: "Summary".to_string(),
                first_kept_message_id: "m3".to_string(),
                compacted_at: 0,
                pre_compaction_tokens: 100,
            }),
            ..Default::default()
        };
        let mut api_msgs = vec![
            Message {
                role: MessageRole::User,
                content: "Q1".to_string(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "A1".to_string(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::User,
                content: "Q2".to_string(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
                reasoning_content: None,
            },
        ];
        shape_compacted_messages(&conv, &mut api_msgs);
        assert_eq!(api_msgs.len(), 1);
        assert_eq!(api_msgs[0].content, "Q2");
        assert!(
            api_msgs[0]
                .context_prefix
                .as_ref()
                .unwrap()
                .contains("<conversation-summary>")
        );
    }

    #[test]
    fn test_shape_compacted_messages_not_found_no_op() {
        let conv = Conversation {
            messages: vec![make_msg("m1", MessageRole::User, "Q1")],
            compaction: Some(CompactionState {
                summary: "Summary".to_string(),
                first_kept_message_id: "missing".to_string(),
                compacted_at: 0,
                pre_compaction_tokens: 100,
            }),
            ..Default::default()
        };
        let mut api_msgs = vec![Message {
            role: MessageRole::User,
            content: "Q1".to_string(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
            reasoning_content: None,
        }];
        shape_compacted_messages(&conv, &mut api_msgs);
        assert_eq!(api_msgs.len(), 1); // untouched
    }

    #[test]
    fn test_compose_context_prefix_both_branches() {
        assert_eq!(compose_context_prefix(None, "a".to_string()), "a");
        assert_eq!(
            compose_context_prefix(Some("x".to_string()), "y".to_string()),
            "x\n\ny"
        );
    }
}
