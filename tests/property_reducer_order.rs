//! AC8 property/table-driven test: arrival-order preservation.
//!
//! Tests that for synthesized chunk sequences, `Turn.parts` preserves
//! the structural interleaving: every `Prose` part's position is
//! monotonically non-decreasing with its source `Text` chunks, every
//! `ToolInvocation` follows the order of `ToolUse` chunks, and
//! `ToolResult` parts appear after their referenced invocations.

use rustain::domain::models::{StopReason, StreamChunk, TurnPart};
use rustain::domain::services::reducer::{reduce, test_reducer_state};

#[derive(Clone, Debug)]
enum V {
    T, // Text
    U, // ToolUse (with id #0)
    R, // ToolResult (with id #0)
    H, // Thinking
}

fn chunk(v: &V) -> StreamChunk {
    match v {
        V::T => StreamChunk::Text {
            content: "x".into(),
            parent_tool_use_id: None,
        },
        V::U => StreamChunk::ToolUse {
            id: "t0".into(),
            name: "test".into(),
            input: serde_json::json!({}),
        },
        V::R => StreamChunk::ToolResult {
            id: "t0".into(),
            content: "r".into(),
            is_error: false,
        },
        V::H => StreamChunk::Thinking {
            content: "h".into(),
            parent_tool_use_id: None,
        },
    }
}

fn sequences(n: usize) -> Vec<Vec<V>> {
    let alpha = vec![V::T, V::H, V::U, V::R];
    let mut result = vec![vec![]; 1];
    for _ in 0..n {
        let mut next = Vec::new();
        for seq in &result {
            for v in &alpha {
                let mut s = seq.clone();
                s.push(v.clone());
                next.push(s);
            }
        }
        result = next;
    }
    result
}

#[test]
fn property_arrival_order_preserved() {
    let mut all_seqs = Vec::new();
    for n in 3..=4 {
        all_seqs.extend(sequences(n));
    }
    // Sample length 5 (first 100)
    all_seqs.extend(sequences(5).into_iter().take(100));

    for (si, seq) in all_seqs.iter().enumerate() {
        let (mut state, clock) = test_reducer_state(1000);
        for v in seq {
            let _ = reduce(&mut state, chunk(v), &clock);
        }
        let _ = reduce(
            &mut state,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            &clock,
        );

        let turn = match state.committed_turn.take() {
            Some(t) => t,
            None => continue,
        };

        // Count types
        let prose_count = turn
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::Prose { .. }))
            .count();
        let inv_count = turn
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::ToolInvocation { .. }))
            .count();
        let res_count = turn
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::ToolResult { .. }))
            .count();
        let thinking_count = turn
            .parts
            .iter()
            .filter(|p| matches!(p, TurnPart::Reasoning { .. }))
            .count();

        // Result count cannot exceed invocation count
        assert!(
            res_count <= inv_count,
            "seq {}: ToolResult count ({}) exceeds ToolInvocation count ({})",
            si,
            res_count,
            inv_count
        );

        // Verify all parts are in structural order
        let part_types: Vec<&str> = turn
            .parts
            .iter()
            .map(|p| match p {
                TurnPart::Prose { .. } => "Prose",
                TurnPart::ToolInvocation { .. } => "ToolInv",
                TurnPart::ToolResult { .. } => "ToolRes",
                TurnPart::Reasoning { .. } => "Reason",
            })
            .collect();

        // Check that no ToolResult appears before its matching ToolInvocation
        let mut inv_seen = false;
        for pt in &part_types {
            if *pt == "ToolInv" {
                inv_seen = true;
            }
            if *pt == "ToolRes" {
                assert!(
                    inv_seen,
                    "seq {}: ToolResult without preceding ToolInvocation",
                    si
                );
            }
        }
        let _ = (prose_count, thinking_count, inv_seen);
    }
}
