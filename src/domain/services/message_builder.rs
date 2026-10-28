use crate::domain::models::{
    ChatMessage, ContentBlockType, Conversation, Message, MessageRole, ProviderReplayItem,
    ResponseSpan, ToolCallInfo, ToolResultMessage, ToolUseMessage, Turn,
};

/// Resolved file content to be attached to a message.
/// Created by the adapter layer (file I/O happens there), consumed by the domain.
#[derive(Debug, Clone, Default)]
pub struct ResolvedFileContext {
    pub path: String,
    pub content: String,
    pub provenance: crate::domain::models::FileContextProvenance,
}
/// Resolved slash command content to be prepended as a system instruction.
/// Created by the adapter layer (file I/O happens there), consumed by the domain.
#[derive(Debug, Clone)]
pub struct ResolvedCommandContext {
    pub name: String,
    pub content: String,
}

/// Escape XML special characters in attribute values.
fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build a context prefix string from resolved file mentions.
/// Returns XML-formatted file content blocks to be prepended to the user message.
pub fn build_file_context_prefix(files: &[ResolvedFileContext]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut prefix = String::new();
    for file in files {
        prefix.push_str(&format!(
            "<file path=\"{}\">\n<![CDATA[{}]]>\n</file>\n\n",
            escape_xml_attr(&file.path),
            file.content.replace("]]>", "]]]]><![CDATA[>")
        ));
    }
    prefix
}

/// Build a context prefix string from a resolved slash command.
/// Returns XML-formatted command content to be prepended to the user message.
pub fn build_command_context_prefix(command: &ResolvedCommandContext) -> String {
    format!(
        "<command name=\"{}\">\n<![CDATA[{}]]>\n</command>\n\n",
        escape_xml_attr(&command.name),
        command.content.replace("]]>", "]]]]><![CDATA[>")
    )
}

/// Build provider API messages from a conversation's chat history.
/// Maps ChatMessage -> Message for each conversation message.
///
/// Tool calls in an assistant message produce a separate user-role message
/// containing the tool results (Anthropic API format).  When the next
/// conversation message is also a User, the tool results are merged INTO
/// that message instead of creating a separate one — this avoids consecutive
/// User messages which violate the Anthropic API's alternating role rule
/// (and would cause the model to silently ignore the second User message).
///
/// Story 20.1d4a T2: an assistant message carrying `ResponseSpan` blocks is
/// rebuilt as one assistant `Message` per span, each followed by its own
/// results, exactly as the turn loop sent them in-turn. Calls are sent by the
/// id the provider gave them.
pub fn build_api_messages(conversation: &Conversation) -> Vec<Message> {
    build_api_messages_for(&conversation.messages, &conversation.turns)
}

/// [`build_api_messages`] over a slice of the message mirror and the turns that
/// back it, so a turn-backed row's spans are checked against its own turn (the
/// windowing assembler's active block).
pub fn build_api_messages_for(chat_messages: &[ChatMessage], turns: &[Turn]) -> Vec<Message> {
    let mut messages = Vec::new();
    // Merge results into an immediately following User message, otherwise flush
    // them before the next assistant/system message to preserve tool adjacency.
    let mut pending_tool_results: Vec<ToolResultMessage> = Vec::new();
    let marked_turns: std::collections::HashMap<&str, &Turn> = turns
        .iter()
        .filter(|turn| !turn.responses.is_empty())
        .map(|turn| (turn.id.0.as_str(), turn))
        .collect();

    for cm in chat_messages {
        if cm.role == MessageRole::Assistant {
            let replay: Vec<&ProviderReplayItem> = cm
                .content_blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlockType::ProviderReplay(item) => Some(item),
                    _ => None,
                })
                .collect();
            if let Some(spans) = response_spans(cm, &replay, &marked_turns) {
                let (mut calls, mut items) = (cm.tool_calls.as_slice(), replay.as_slice());
                for span in spans {
                    let (own_calls, rest) = calls.split_at(span.tool_calls);
                    let (own_items, rest_items) = items.split_at(span.replay);
                    (calls, items) = (rest, rest_items);
                    push_assistant(
                        &mut messages,
                        &mut pending_tool_results,
                        span.text.clone(),
                        span.reasoning.clone(),
                        own_calls,
                        own_items,
                    );
                }
                continue;
            }
            // Extract thinking/reasoning content from content_blocks so
            // providers like DeepSeek v4 that require it can echo it back.
            let reasoning_content = cm.content_blocks.iter().find_map(|b| match b {
                ContentBlockType::Thinking(text) => Some(text.clone()),
                _ => None,
            });
            push_assistant(
                &mut messages,
                &mut pending_tool_results,
                cm.content.clone(),
                reasoning_content,
                &cm.tool_calls,
                &replay,
            );
        } else {
            if cm.role != MessageRole::User && !pending_tool_results.is_empty() {
                messages.push(tool_results_message(std::mem::take(
                    &mut pending_tool_results,
                )));
            }
            // User (or System) message: attach any buffered tool results
            let tool_results = std::mem::take(&mut pending_tool_results);
            messages.push(Message {
                role: cm.role,
                content: cm.content.clone(),
                images: vec![],
                tool_results,
                tool_uses: vec![],
                context_prefix: None,
                reasoning_content: None,
                provider_replay: vec![],
            });
        }
    }

    // Flush any remaining pending tool results (edge case: last message was Assistant)
    if !pending_tool_results.is_empty() {
        messages.push(tool_results_message(pending_tool_results));
    }

    messages
}

/// One assistant `Message` and its results, queued for the next flush.
fn push_assistant(
    messages: &mut Vec<Message>,
    pending_tool_results: &mut Vec<ToolResultMessage>,
    content: String,
    reasoning_content: Option<String>,
    calls: &[ToolCallInfo],
    replay: &[&ProviderReplayItem],
) {
    // For assistant messages with tool calls, include tool_use blocks
    // so the Anthropic API can match tool_results to their originating calls.
    let tool_uses: Vec<ToolUseMessage> = calls
        .iter()
        .filter(|tc| tc.result.is_some())
        .map(|tc| ToolUseMessage {
            id: tc.wire_id().to_owned(),
            name: tc.name.clone(),
            input: tc.input.clone(),
        })
        .collect();
    // Interrupted calls remain in the audit history, but an unmatched
    // invocation (or its otherwise empty row) is not a valid API message.
    // Replay state alone never keeps a row: other providers drop it.
    if content.is_empty() && tool_uses.is_empty() && reasoning_content.is_none() {
        return;
    }
    if !pending_tool_results.is_empty() {
        messages.push(tool_results_message(std::mem::take(pending_tool_results)));
    }
    messages.push(Message {
        role: MessageRole::Assistant,
        content,
        images: vec![],
        tool_results: vec![],
        tool_uses,
        context_prefix: None,
        reasoning_content,
        provider_replay: replay.iter().map(|item| (*item).clone()).collect(),
    });
    pending_tool_results.extend(calls.iter().filter_map(|tc| {
        tc.result.as_ref().map(|r| ToolResultMessage {
            tool_use_id: tc.wire_id().to_owned(),
            content: r.content.clone(),
            is_error: r.is_error,
            images: r.images.clone(),
        })
    }));
}

/// The message's response spans when they still describe it exactly: their
/// counts cover its calls and replay items, and its text is what its persister
/// wrote (the turn mirror's display fold for a turn-backed row, otherwise the
/// spans' visible text). An edited or inconsistent row rebuilds as one
/// message, as before Story 20.1d4a.
fn response_spans<'a>(
    cm: &'a ChatMessage,
    replay: &[&ProviderReplayItem],
    marked_turns: &std::collections::HashMap<&str, &Turn>,
) -> Option<Vec<&'a ResponseSpan>> {
    let spans: Vec<&ResponseSpan> = cm
        .content_blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlockType::ResponseSpan(span) => Some(span),
            _ => None,
        })
        .collect();
    // Counts come from the session file: a corrupt or crafted one must fall
    // back rather than overflow into a panicking split.
    let calls = spans
        .iter()
        .try_fold(0_usize, |sum, span| sum.checked_add(span.tool_calls));
    let items = spans
        .iter()
        .try_fold(0_usize, |sum, span| sum.checked_add(span.replay));
    if spans.is_empty() || calls != Some(cm.tool_calls.len()) || items != Some(replay.len()) {
        return None;
    }
    let written = match marked_turns.get(cm.id.as_str()) {
        Some(turn) => turn.display_text(),
        None => spans.iter().map(|span| span.text.as_str()).collect(),
    };
    (written == cm.content).then_some(spans)
}

fn tool_results_message(tool_results: Vec<ToolResultMessage>) -> Message {
    Message {
        role: MessageRole::User,
        content: String::new(),
        images: vec![],
        tool_results,
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
        provider_replay: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ImageAttachment, ToolResultInfo, WriteDiffState};

    fn item(marker: &str) -> ProviderReplayItem {
        ProviderReplayItem {
            provider: "p".into(),
            model: "m".into(),
            payload: std::sync::Arc::new(serde_json::json!({"m": marker})),
            scope: Some("s".into()),
        }
    }

    fn image() -> ImageAttachment {
        ImageAttachment {
            media_type: "image/png".into(),
            data: "UE5H".into(),
        }
    }

    fn call(id: &str, provider: Option<&str>, result: Option<&str>) -> ToolCallInfo {
        ToolCallInfo {
            id: id.into(),
            name: "tool".into(),
            input: serde_json::json!({"id": id}),
            result: result.map(|content| ToolResultInfo {
                content: content.into(),
                is_error: false,
                images: if content == "img" {
                    vec![image()]
                } else {
                    vec![]
                },
                diff: WriteDiffState::NotAWrite,
            }),
            started_at_ms: None,
            completed_at_ms: None,
            status: None,
            provider_id: provider.map(str::to_owned),
        }
    }

    fn span(
        text: &str,
        reasoning: Option<&str>,
        tool_calls: usize,
        replay: usize,
    ) -> ContentBlockType {
        ContentBlockType::ResponseSpan(ResponseSpan {
            text: text.into(),
            reasoning: reasoning.map(str::to_owned),
            tool_calls,
            replay,
        })
    }

    fn conversation(messages: Vec<ChatMessage>) -> Conversation {
        Conversation {
            messages,
            ..Default::default()
        }
    }

    fn user(text: &str) -> ChatMessage {
        ChatMessage {
            role: MessageRole::User,
            content: text.into(),
            ..Default::default()
        }
    }

    /// Two responses persisted as one row (`ask`): call A + B with replay R1,
    /// then call C with replay R2, then the final text with replay R3.
    fn spanned_row() -> ChatMessage {
        ChatMessage {
            id: "row".into(),
            role: MessageRole::Assistant,
            content: "Vdone".into(),
            content_blocks: vec![
                ContentBlockType::ProviderReplay(item("r1")),
                ContentBlockType::ProviderReplay(item("r2")),
                ContentBlockType::ProviderReplay(item("r3")),
                span("V", Some("T"), 2, 1),
                span("", None, 1, 1),
                span("done", None, 0, 1),
            ],
            tool_calls: vec![
                call("tc_x_1", Some("A"), Some("img")),
                call("tc_x_2", Some("B"), Some("b")),
                call("tc_x_3", Some("C"), Some("c")),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn spans_rebuild_one_message_per_response_with_its_own_calls_results_and_replay() {
        let api = build_api_messages(&conversation(vec![user("go"), spanned_row(), user("next")]));
        let shape: Vec<_> = api
            .iter()
            .map(|m| {
                (
                    m.role,
                    m.content.as_str(),
                    m.reasoning_content.as_deref(),
                    m.tool_uses
                        .iter()
                        .map(|u| u.id.as_str())
                        .collect::<Vec<_>>(),
                    m.tool_results
                        .iter()
                        .map(|r| r.tool_use_id.as_str())
                        .collect::<Vec<_>>(),
                    m.provider_replay.len(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (MessageRole::User, "go", None, vec![], vec![], 0),
                (
                    MessageRole::Assistant,
                    "V",
                    Some("T"),
                    vec!["A", "B"],
                    vec![],
                    1
                ),
                (MessageRole::User, "", None, vec![], vec!["A", "B"], 0),
                (MessageRole::Assistant, "", None, vec!["C"], vec![], 1),
                (MessageRole::User, "", None, vec![], vec!["C"], 0),
                (MessageRole::Assistant, "done", None, vec![], vec![], 1),
                (MessageRole::User, "next", None, vec![], vec![], 0),
            ]
        );
        assert_eq!(api[1].provider_replay[0], item("r1"));
        assert_eq!(api[3].provider_replay[0], item("r2"));
        assert_eq!(api[5].provider_replay[0], item("r3"));
        assert_eq!(api[2].tool_results[0].images, vec![image()]);
        assert!(api[2].tool_results[1].images.is_empty());
    }

    #[test]
    fn a_call_without_its_result_is_repaired_per_span() {
        let mut row = spanned_row();
        row.tool_calls[1].result = None;
        let api = build_api_messages(&conversation(vec![row]));
        let uses: Vec<_> = api[0].tool_uses.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(uses, vec!["A"]);
        let results: Vec<_> = api[1]
            .tool_results
            .iter()
            .map(|r| r.tool_use_id.as_str())
            .collect();
        assert_eq!(results, vec!["A"]);
    }

    #[test]
    fn replay_only_span_is_skipped_like_a_replay_only_row() {
        let row = ChatMessage {
            role: MessageRole::Assistant,
            content: "x".into(),
            content_blocks: vec![
                ContentBlockType::ProviderReplay(item("r1")),
                ContentBlockType::ProviderReplay(item("r2")),
                span("", None, 0, 1),
                span("x", None, 0, 1),
            ],
            ..Default::default()
        };
        let api = build_api_messages(&conversation(vec![row]));
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].content, "x");
        assert_eq!(api[0].provider_replay, vec![item("r2")]);
    }

    #[test]
    fn an_edited_or_inconsistent_row_rebuilds_as_one_message_by_provider_id() {
        for edit in [
            |row: &mut ChatMessage| row.content = "edited".into(),
            |row: &mut ChatMessage| row.content_blocks.pop().map(drop).unwrap_or(()),
            |row: &mut ChatMessage| {
                row.tool_calls.pop();
            },
        ] {
            let mut row = spanned_row();
            edit(&mut row);
            let api = build_api_messages(&conversation(vec![row.clone()]));
            assert_eq!(api[0].content, row.content);
            assert_eq!(
                api[0].provider_replay.len(),
                3,
                "every item rides the one row"
            );
            assert_eq!(
                api[0].tool_uses[0].id, "A",
                "provider ids survive the fallback"
            );
        }
    }

    #[test]
    fn a_row_without_spans_or_provider_ids_rebuilds_as_before() {
        let row = ChatMessage {
            role: MessageRole::Assistant,
            content: "text".into(),
            content_blocks: vec![ContentBlockType::Thinking("think".into())],
            tool_calls: vec![call("tc_1", None, Some("r")), call("tc_2", None, None)],
            ..Default::default()
        };
        let api = build_api_messages(&conversation(vec![row, user("next")]));
        assert_eq!(api.len(), 2);
        assert_eq!(api[0].content, "text");
        assert_eq!(api[0].reasoning_content.as_deref(), Some("think"));
        assert_eq!(api[0].tool_uses.len(), 1);
        assert_eq!(api[0].tool_uses[0].id, "tc_1");
        assert_eq!(api[1].tool_results[0].tool_use_id, "tc_1");
        assert_eq!(api[1].content, "next");
    }

    #[test]
    fn a_recycled_provider_id_keeps_each_result_with_its_own_response() {
        let row = ChatMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            content_blocks: vec![span("", None, 1, 0), span("", None, 1, 0)],
            tool_calls: vec![
                call("tc_1", Some("same"), Some("first")),
                call("tc_2", Some("same"), Some("second")),
            ],
            ..Default::default()
        };
        let api = build_api_messages(&conversation(vec![row]));
        assert_eq!(api.len(), 4);
        assert_eq!(api[1].tool_results[0].content, "first");
        assert_eq!(api[3].tool_results[0].content, "second");
    }

    /// Review RF-09: span counts from a corrupt file whose sum overflows fall
    /// back to the one-message rebuild instead of panicking.
    #[test]
    fn overflowing_span_counts_fall_back_instead_of_panicking() {
        let mut row = spanned_row();
        row.content_blocks
            .retain(|block| !matches!(block, ContentBlockType::ResponseSpan(_)));
        row.content_blocks.push(span("V", None, usize::MAX, 0));
        row.content_blocks.push(span("done", None, 4, 3));
        let api = build_api_messages(&conversation(vec![row]));
        assert_eq!(api[0].content, "Vdone");
        assert_eq!(api[0].tool_uses.len(), 3);
        assert_eq!(api[0].provider_replay.len(), 3);
    }
}
