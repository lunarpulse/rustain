//! AC7 regression test: verify the `concat_prose_2026_04_22` bug pattern
//! (prose before and between tool invocations) is preserved by the new reducer.
//!
//! The legacy `apply_chunk` would concat all prose into one block then all
//! tool calls. The new reducer preserves interleaving: ≥3 distinct Prose parts,
//! first Prose precedes first ToolInvocation, and ToolResult.refs matches.

use rustain::domain::models::{InvocationStatus, StopReason, StreamChunk, TurnPart};
use rustain::domain::services::reducer::{reduce, test_reducer_state};

/// Mirror the pattern recorded in `tests/fixtures/regression/concat_prose_2026_04_22.jsonl`
/// (4 prose runs, 6 tool calls, interleaved). Uses inline chunks because the fixture's
/// camelCase field names don't match `StreamChunk`'s snake_case serde format.
fn regression_chunks() -> Vec<StreamChunk> {
    use StreamChunk::*;
    vec![
        // Prose run 1: 4 text deltas
        Text {
            content: "Understood".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " — let me put".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " curl through its".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " paces.".into(),
            parent_tool_use_id: None,
        },
        // ToolUse 1 + ToolResult 1
        ToolUse {
            id: "toolu_01_curl_a2a_spec".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command":"curl","description":"Fetch A2A"}),
        },
        ToolResult {
            id: "toolu_01_curl_a2a_spec".into(),
            content: "spec".into(),
            is_error: false,
        },
        // Prose run 2: 5 text deltas
        Text {
            content: "I'll hit".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " the key sources".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " for A2A protocol".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " research".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " simultaneously.".into(),
            parent_tool_use_id: None,
        },
        // ToolUse 2, 3, 4 (batched)
        ToolUse {
            id: "toolu_02".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        },
        ToolUse {
            id: "toolu_03".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        },
        ToolUse {
            id: "toolu_04".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        },
        // ToolResults for 2, 3, 4
        ToolResult {
            id: "toolu_02".into(),
            content: "MCP intro".into(),
            is_error: false,
        },
        ToolResult {
            id: "toolu_03".into(),
            content: "A2A key concepts".into(),
            is_error: false,
        },
        ToolResult {
            id: "toolu_04".into(),
            content: "Agent Skills".into(),
            is_error: false,
        },
        // Prose run 3: 5 text deltas
        Text {
            content: "Good".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " — curl is working".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " and we're".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " pulling live".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " data.".into(),
            parent_tool_use_id: None,
        },
        // ToolUse 5, 6
        ToolUse {
            id: "toolu_05".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        },
        ToolUse {
            id: "toolu_06".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        },
        // ToolResults for 5, 6
        ToolResult {
            id: "toolu_05".into(),
            content: "discovery".into(),
            is_error: false,
        },
        ToolResult {
            id: "toolu_06".into(),
            content: "architecture".into(),
            is_error: false,
        },
        // Prose run 4: 6 text deltas
        Text {
            content: "Let me get".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " the official docs".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " and read".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " them".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " carefully before".into(),
            parent_tool_use_id: None,
        },
        Text {
            content: " drafting the design.".into(),
            parent_tool_use_id: None,
        },
        TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

#[test]
fn regression_concat_prose_interleaving_preserved() {
    let chunks = regression_chunks();
    let (mut state, clock) = test_reducer_state(1000);

    for chunk in chunks {
        let _ = reduce(&mut state, chunk, &clock);
    }

    let turn = state
        .committed_turn
        .take()
        .expect("should produce a committed turn");

    // AC7: ≥3 distinct Prose parts
    let prose_parts: Vec<&TurnPart> = turn
        .parts
        .iter()
        .filter(|p| matches!(p, TurnPart::Prose { .. }))
        .collect();
    assert!(
        prose_parts.len() >= 3,
        "expected >=3 prose parts, got {}",
        prose_parts.len()
    );

    // AC7: first Prose precedes first ToolInvocation
    let first_prose_idx = turn
        .parts
        .iter()
        .position(|p| matches!(p, TurnPart::Prose { .. }))
        .expect("should have at least one Prose part");
    let first_invocation_idx = turn
        .parts
        .iter()
        .position(|p| matches!(p, TurnPart::ToolInvocation { .. }))
        .expect("should have at least one ToolInvocation part");
    assert!(
        first_prose_idx < first_invocation_idx,
        "first Prose must precede first ToolInvocation"
    );

    // AC7: at least one ToolResult.refs matches an invocation PartId
    let invocation_ids: Vec<_> = turn
        .parts
        .iter()
        .filter_map(|p| {
            if let TurnPart::ToolInvocation { id, .. } = p {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    let mut result_refs_found = false;
    for part in &turn.parts {
        if let TurnPart::ToolResult { refs, .. } = part {
            if invocation_ids.contains(refs) {
                result_refs_found = true;
                break;
            }
        }
    }
    assert!(
        result_refs_found,
        "at least one ToolResult must reference a ToolInvocation PartId"
    );
}
