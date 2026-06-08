//! Story 11.6 AC-11.6.4 — causal-chain integrity property test.
//!
//! Over ≥256 generated conversations (mixed user/assistant turns with tool
//! invocation/result parts), assembled under a range of budgets, the emitted
//! `Vec<Message>` must contain **zero orphan tool-results**: every
//! `ToolResultMessage.tool_use_id` has a matching `ToolUseMessage.id` somewhere
//! in the same bundle. This is the wire-level proof of the "structurally
//! lossless trim" property — `build_api_messages` re-splits a turn's
//! tool_results onto the following user message, so the assertion is against the
//! EMITTED messages, not the turn structure.
//!
//! NOT feature-gated — the windowing assembler is in the default build.

use proptest::prelude::*;
use rustain::domain::models::turn::{InvocationStatus, Turn, TurnPart};
use rustain::domain::models::{
    AssemblyBudget, ChatMessage, Conversation, MessageRole, ToolCallInfo, ToolResultInfo,
    generate_conversation_id,
};
use rustain::domain::ports::ContextAssemblerPort;
use rustain::infrastructure::context::WindowingAssembler;
use serde_json::json;
use std::collections::BTreeSet;

const TOOLS: &[&str] = &["Read", "Edit", "Bash", "Grep"];
const PATHS: &[&str] = &[
    "src/auth/a.rs",
    "src/auth/b.rs",
    "src/parser/c.rs",
    "src/parser/d.rs",
    "docs/e.md",
];

/// One generated tool part: (tool index, path index, resolved?).
type ToolSpec = (usize, usize, bool);
/// One generated turn: (gap-minutes before this turn, tool specs).
type TurnSpec = (u32, Vec<ToolSpec>);

fn build_conversation(specs: &[TurnSpec]) -> Conversation {
    let mut turns = Vec::new();
    let mut clock: i64 = 0;
    for (gap, tools) in specs {
        clock += (*gap as i64) * 60_000 + 1000;
        let mut t = Turn::new("m".into(), clock);
        t.push_part(|id| TurnPart::Prose {
            id,
            text: "some work was done here".into(),
        });
        for (ti, pi, resolved) in tools {
            let tool = TOOLS[ti % TOOLS.len()].to_string();
            let path = PATHS[pi % PATHS.len()].to_string();
            let inv = t.push_part(|id| TurnPart::ToolInvocation {
                id,
                tool,
                args: json!({ "file_path": path }),
                status: if *resolved {
                    InvocationStatus::Success
                } else {
                    InvocationStatus::Running
                },
                started_at: clock,
                ended_at: if *resolved { Some(clock + 1) } else { None },
            });
            if *resolved {
                t.push_part(|id| TurnPart::ToolResult {
                    id,
                    refs: inv,
                    output: rustain::domain::models::turn::ToolOutput {
                        content: "ok".into(),
                        is_error: false,
                    },
                });
            }
        }
        turns.push(t);
    }

    // Build the interleaved message mirror (user prompt + assistant per turn),
    // plus a trailing live prompt — mirrors the live event loop.
    let mut messages = Vec::new();
    for (i, t) in turns.iter().enumerate() {
        messages.push(user_chat(&format!("prompt {i}")));
        messages.push(assistant_chat(t));
    }
    messages.push(user_chat("current prompt"));

    Conversation {
        messages,
        turns,
        session_id: Some("prop".into()),
        ..Default::default()
    }
}

fn user_chat(content: &str) -> ChatMessage {
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
        origin: rustain::domain::models::ChannelKind::Terminal,
    }
}

fn assistant_chat(t: &Turn) -> ChatMessage {
    let mut content = String::new();
    let mut tool_calls = vec![];
    for part in &t.parts {
        match part {
            TurnPart::Prose { text, .. } => content.push_str(text),
            TurnPart::ToolInvocation {
                id,
                tool,
                args,
                status,
                ..
            } => tool_calls.push(ToolCallInfo {
                id: format!("tc_{}_{}", t.id.0, id.0),
                name: tool.clone(),
                input: args.clone(),
                // Resolved invocations carry a result; pending ones do not (so
                // build_api_messages emits a tool_use with no tool_result — a
                // legal pending call, not an orphan result).
                result: if matches!(status, InvocationStatus::Success) {
                    Some(ToolResultInfo {
                        content: "ok".into(),
                        is_error: false,
                    })
                } else {
                    None
                },
                started_at_ms: Some(0),
                completed_at_ms: Some(1),
                status: Some("✓".into()),
            }),
            _ => {}
        }
    }
    ChatMessage {
        synthetic: false,
        id: t.id.0.clone(),
        role: MessageRole::Assistant,
        content,
        content_blocks: vec![],
        tool_calls,
        created_at: 0,
        token_count: None,
        stop_reason: None,
        images: vec![],
        origin: rustain::domain::models::ChannelKind::Terminal,
    }
}

fn turn_spec_strategy() -> impl Strategy<Value = TurnSpec> {
    let tool_spec = (0usize..4, 0usize..5, any::<bool>());
    let gap = 0u32..40;
    (gap, prop::collection::vec(tool_spec, 0..4))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Across random conversations and a range of budgets, no tool-result is
    /// emitted without its originating tool-use in the same bundle.
    #[test]
    fn no_orphan_tool_results_across_budgets(
        specs in prop::collection::vec(turn_spec_strategy(), 1..14),
    ) {
        let conv = build_conversation(&specs);
        let asm = WindowingAssembler::default();

        for max_tokens in [0usize, 25, 100, 500, usize::MAX] {
            let out = asm.assemble(&conv, AssemblyBudget { max_tokens });

            // Collect every tool_use id present in the emitted bundle.
            let tool_use_ids: BTreeSet<String> = out
                .messages
                .iter()
                .flat_map(|m| m.tool_uses.iter().map(|tu| tu.id.clone()))
                .collect();

            // Every tool_result must reference a tool_use present in the bundle.
            for m in &out.messages {
                for tr in &m.tool_results {
                    prop_assert!(
                        tool_use_ids.contains(&tr.tool_use_id),
                        "orphan tool_result {} at budget {} (uses present: {:?})",
                        tr.tool_use_id,
                        max_tokens,
                        tool_use_ids,
                    );
                }
            }

            // The active block always survives; an empty conversation aside, the
            // bundle is never empty when there are turns.
            if !conv.turns.is_empty() {
                prop_assert!(!out.messages.is_empty());
            }
        }
    }
}
