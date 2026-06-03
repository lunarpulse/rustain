//! [`StaticPassthroughAssembler`] — the behaviour-preserving default
//! [`ContextAssemblerPort`] (Story 11.0a, impl #1).
//!
//! It is the de-facto passthrough that was inline in `start_turn_inner` before
//! the seam existed: it delegates to the pure
//! `domain::services::message_builder::build_api_messages` and returns its
//! `Vec<Message>` **byte-identical**, with empty diagnostics. The `budget`
//! argument is ignored (Story 11.6's `WindowingAssembler` is the impl that
//! honours it).

use crate::domain::models::{AssembledContext, AssemblyBudget, Conversation};
use crate::domain::ports::ContextAssemblerPort;
use crate::domain::services::message_builder::build_api_messages;

/// The behaviour-preserving default Message-tier assembler. Unit struct — it
/// holds no state and does no I/O.
#[derive(Debug, Clone, Copy, Default)]
pub struct StaticPassthroughAssembler;

impl ContextAssemblerPort for StaticPassthroughAssembler {
    /// Byte-identical passthrough: `build_api_messages(conversation)` with
    /// `AssembleDiagnostics::default()`. `budget` is ignored.
    fn assemble(&self, conversation: &Conversation, _budget: AssemblyBudget) -> AssembledContext {
        AssembledContext {
            messages: build_api_messages(conversation),
            diagnostics: crate::domain::models::AssembleDiagnostics::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        AssembleDiagnostics, ChatMessage, ContentBlockType, Conversation, Message, MessageRole,
        ToolCallInfo, ToolResultInfo, generate_conversation_id,
    };

    fn make_conversation(messages: Vec<ChatMessage>) -> Conversation {
        Conversation {
            id: "test".to_string(),
            title: String::new(),
            messages,
            turns: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
        }
    }

    fn user(content: &str) -> ChatMessage {
        ChatMessage {
            synthetic: false,
            id: generate_conversation_id(),
            role: MessageRole::User,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        }
    }

    fn assistant(content: &str, tool_calls: Vec<ToolCallInfo>) -> ChatMessage {
        ChatMessage {
            synthetic: false,
            id: generate_conversation_id(),
            role: MessageRole::Assistant,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls,
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        }
    }

    fn assistant_thinking(content: &str, thinking: &str) -> ChatMessage {
        ChatMessage {
            synthetic: false,
            id: generate_conversation_id(),
            role: MessageRole::Assistant,
            content: content.to_string(),
            content_blocks: vec![ContentBlockType::Thinking(thinking.to_string())],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        }
    }

    fn tool_call(id: &str, name: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({"k": "v"}),
            result: Some(ToolResultInfo {
                content: "ok".to_string(),
                is_error: false,
            }),
            started_at_ms: Some(0),
            completed_at_ms: Some(1),
            status: Some("✓ Success".to_string()),
        }
    }

    /// `Message` is not `PartialEq` (it is the provider wire type), so byte
    /// identity is asserted via the serialized wire payload — exactly the
    /// "byte-identical wire payload" AC2/AC5 require.
    fn wire(messages: &[Message]) -> serde_json::Value {
        serde_json::to_value(messages).expect("Message serializes")
    }

    /// The representative fixture set named in AC5: empty; user-only;
    /// assistant-with-tool-calls (pending tool-results merged into next user);
    /// trailing-assistant flush; a `Thinking` block driving `reasoning_content`.
    fn fixtures() -> Vec<Conversation> {
        vec![
            // empty conversation
            make_conversation(vec![]),
            // user-only
            make_conversation(vec![user("hello")]),
            // assistant-with-tool-calls producing pending tool-results, then a
            // following user message they merge into
            make_conversation(vec![
                user("use a tool"),
                assistant("on it", vec![tool_call("tc-1", "bash")]),
                user("thanks"),
            ]),
            // trailing-assistant flush (last message is assistant w/ tool calls →
            // build_api_messages appends a synthetic user with the flushed results)
            make_conversation(vec![
                user("use a tool"),
                assistant("on it", vec![tool_call("tc-2", "read")]),
            ]),
            // Thinking block driving reasoning_content echo-back (DeepSeek v4)
            make_conversation(vec![
                user("think"),
                assistant_thinking("answer", "deliberating..."),
            ]),
        ]
    }

    /// AC2 + AC5 (linchpin): the passthrough's `.messages` are byte-for-byte the
    /// pre-port `build_api_messages` output, and `.diagnostics` is `default()`.
    #[test]
    fn passthrough_messages_byte_identical_to_build_api_messages() {
        let assembler = StaticPassthroughAssembler;
        let budget = AssemblyBudget {
            max_tokens: usize::MAX,
        };
        for conv in fixtures() {
            let assembled = assembler.assemble(&conv, budget);
            let baseline = build_api_messages(&conv);
            assert_eq!(
                wire(&assembled.messages),
                wire(&baseline),
                "passthrough wire payload must equal build_api_messages for fixture {:?}",
                conv.messages.iter().map(|m| &m.content).collect::<Vec<_>>()
            );
            assert_eq!(
                assembled.diagnostics,
                AssembleDiagnostics::default(),
                "passthrough carries no diagnostics"
            );
        }
    }

    /// AC2: the `budget` argument is ignored — a tiny budget changes nothing.
    #[test]
    fn passthrough_ignores_budget() {
        let assembler = StaticPassthroughAssembler;
        let conv = make_conversation(vec![user("a"), assistant("b", vec![]), user("c")]);
        let big = assembler.assemble(
            &conv,
            AssemblyBudget {
                max_tokens: usize::MAX,
            },
        );
        let tiny = assembler.assemble(&conv, AssemblyBudget { max_tokens: 0 });
        assert_eq!(wire(&big.messages), wire(&tiny.messages));
    }

    /// AC5 (second test): pins the `None`-slot eval/replay fallback — which calls
    /// `build_api_messages` directly — equal to the passthrough `.messages`, so
    /// the branch cannot rot before Story 11.6 makes it observable.
    #[test]
    fn none_slot_fallback_equals_passthrough_messages() {
        let assembler = StaticPassthroughAssembler;
        let budget = AssemblyBudget {
            max_tokens: usize::MAX,
        };
        for conv in fixtures() {
            // `Some(assembler)` path (TUI default)
            let some_path = assembler.assemble(&conv, budget).messages;
            // `None` path (eval/replay bypass) — event_loop falls back to this
            let none_path = build_api_messages(&conv);
            assert_eq!(wire(&some_path), wire(&none_path));
        }
    }
}
