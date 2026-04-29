//! AC6 conformance test: replay interleaved streaming JSONL through `reduce()`.
//!
//! Validates that the reducer produces the correct `Turn.parts` for a known
//! interleaved sequence containing prose, tool invocations, and tool results.
//!
//! Pattern after `tests/conformance_plan_runtime.rs` and `tests/anthropic_streaming.rs`.

use std::fs;

use rustain::domain::events::ChunkAction;
use rustain::domain::models::{
    InvocationStatus, PartId, StopReason, StreamChunk, ToolOutput, TurnPart,
};
use rustain::domain::services::reducer::{test_reducer_state, reduce};

#[test]
fn conformance_interleaved_streaming_produces_correct_parts() {
    let fixture_path = "tests/fixtures/conformance/interleaved_streaming.jsonl";
    let content = fs::read_to_string(fixture_path).expect("fixture should exist");

    let (mut state, clock) = test_reducer_state(1000);

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let chunk: StreamChunk =
            serde_json::from_str(line).expect("each line should be a valid StreamChunk");

        let _ = reduce(&mut state, chunk, &clock);
    }

    // After the final TurnComplete, committed_turn has the finished parts
    let turn = state
        .committed_turn
        .take()
        .expect("final chunk should have committed a turn");

    // Expected parts:
    // 1. Prose "I'll search" (part 0)
    // 2. ToolInvocation "Bash" (part 1)
    // 3. ToolResult for Bash (part 2)
    let expected = vec![
        TurnPart::Prose {
            id: PartId(0),
            text: "I'll search".into(),
        },
        TurnPart::ToolInvocation {
            id: PartId(1),
            tool: "Bash".into(),
            args: serde_json::json!({"command": "ls"}),
            status: InvocationStatus::Success,
            started_at: 1000,
            ended_at: Some(1000),
        },
        TurnPart::ToolResult {
            id: PartId(2),
            refs: PartId(1),
            output: ToolOutput {
                content: "output".into(),
                is_error: false,
            },
        },
    ];

    assert_eq!(
        turn.parts.len(),
        expected.len(),
        "part count mismatch: expected {}, got {}",
        expected.len(),
        turn.parts.len()
    );

    for (i, (actual, expected)) in turn.parts.iter().zip(expected.iter()).enumerate() {
        match (actual, expected) {
            (TurnPart::Prose { text: a, .. }, TurnPart::Prose { text: e, .. }) => {
                assert_eq!(a, e, "prose text mismatch at part {i}");
            }
            (TurnPart::ToolInvocation { tool: a, .. }, TurnPart::ToolInvocation { tool: e, .. }) => {
                assert_eq!(a, e, "tool name mismatch at part {i}");
            }
            (TurnPart::ToolResult { output: a, .. }, TurnPart::ToolResult { output: e, .. }) => {
                assert_eq!(a.content, e.content, "result content mismatch at part {i}");
            }
            _ => panic!("unexpected part variant at {i}"),
        }
    }

    // AC6: interleaving preserved — Prose before ToolInvocation
    let first_prose_idx = turn
        .parts
        .iter()
        .position(|p| matches!(p, TurnPart::Prose { .. }))
        .unwrap();
    let first_inv_idx = turn
        .parts
        .iter()
        .position(|p| matches!(p, TurnPart::ToolInvocation { .. }))
        .unwrap();
    assert!(
        first_prose_idx < first_inv_idx,
        "first Prose (idx {}) must precede first ToolInvocation (idx {})",
        first_prose_idx,
        first_inv_idx
    );
}
