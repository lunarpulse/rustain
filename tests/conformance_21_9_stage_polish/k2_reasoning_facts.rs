//! K2: the reducer records truthful reasoning facts (D1-D3, PC-1, PC-5): the
//! committed part order is the chunk-run order, the duration spans the first
//! thinking chunk to its flush, provider tokens attach to the one reasoning part
//! of the open response only, and the schema stays additive.
use std::time::Duration;

use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    Conversation, PartId, StopReason, StreamChunk, Turn, TurnPart, UsageInfo,
};
use rustain::domain::services::reducer::{ReducerState, reduce, test_reducer_state};

fn think(text: &str) -> StreamChunk {
    StreamChunk::Thinking {
        content: text.into(),
        parent_tool_use_id: None,
    }
}

fn say(text: &str) -> StreamChunk {
    StreamChunk::Text {
        content: text.into(),
        parent_tool_use_id: None,
    }
}

fn usage(reasoning: Option<u32>) -> StreamChunk {
    StreamChunk::Usage {
        usage: UsageInfo {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: reasoning,
        },
        session_id: None,
    }
}

fn tool_use(id: &str) -> StreamChunk {
    StreamChunk::ToolUse {
        id: id.into(),
        name: "test".into(),
        input: serde_json::json!({}),
    }
}

fn tool_result(id: &str) -> StreamChunk {
    StreamChunk::ToolResult {
        id: id.into(),
        content: "r".into(),
        is_error: false,
        operator_private: false,
        diff: rustain::domain::models::WriteDiffState::NotAWrite,
        images: Vec::new(),
    }
}

fn end(reason: StopReason) -> StreamChunk {
    StreamChunk::TurnComplete {
        stop_reason: reason,
    }
}

fn run(chunks: Vec<StreamChunk>) -> (ReducerState, MockClock) {
    let (mut state, clock) = test_reducer_state(1_000);
    for chunk in chunks {
        let _ = reduce(&mut state, chunk, &clock);
    }
    (state, clock)
}

fn kinds(turn: &Turn) -> Vec<&'static str> {
    turn.parts
        .iter()
        .map(|part| match part {
            TurnPart::Prose { .. } => "prose",
            TurnPart::Reasoning { .. } => "reasoning",
            TurnPart::ToolInvocation { .. } => "call",
            TurnPart::ToolResult { .. } => "result",
            TurnPart::ProviderReplay { .. } => "replay",
        })
        .collect()
}

fn reasoning_facts(turn: &Turn) -> Vec<(String, Option<u64>, Option<u32>)> {
    turn.parts
        .iter()
        .filter_map(|part| match part {
            TurnPart::Reasoning {
                text,
                duration_ms,
                tokens,
                ..
            } => Some((text.clone(), *duration_ms, *tokens)),
            _ => None,
        })
        .collect()
}

#[test]
fn thinking_then_text_commits_reasoning_first() {
    let (mut state, _) = run(vec![think("plan"), say("answer"), end(StopReason::EndTurn)]);
    let turn = state.committed_turn.take().expect("committed");
    assert_eq!(kinds(&turn), ["reasoning", "prose"]);
    // Control: text then thinking keeps its own order.
    let (mut state, _) = run(vec![say("answer"), think("plan"), end(StopReason::EndTurn)]);
    assert_eq!(
        kinds(&state.committed_turn.take().unwrap()),
        ["prose", "reasoning"]
    );
}

#[test]
fn interleaved_thinking_yields_one_part_per_run() {
    let (mut state, _) = run(vec![
        think("a"),
        say("b"),
        think("c"),
        end(StopReason::EndTurn),
    ]);
    let turn = state.committed_turn.take().unwrap();
    assert_eq!(kinds(&turn), ["reasoning", "prose", "reasoning"]);
    let texts: Vec<_> = reasoning_facts(&turn).into_iter().map(|f| f.0).collect();
    assert_eq!(texts, ["a", "c"], "never merged across the answer");
}

#[derive(Clone, Copy, Debug)]
enum V {
    T,
    H,
    U,
    R,
}

fn chunk(v: V) -> StreamChunk {
    match v {
        V::T => say("x"),
        V::H => think("h"),
        V::U => tool_use("t0"),
        V::R => tool_result("t0"),
    }
}

/// The parts an observer of the chunk runs expects, written independently of
/// the reducer: a run of the same kind is one part, any other chunk closes it.
fn expected_kinds(seq: &[V]) -> Vec<&'static str> {
    let mut parts: Vec<&'static str> = Vec::new();
    let mut open: Option<&'static str> = None;
    let mut pending = false;
    for v in seq {
        match v {
            V::T | V::H => {
                let kind = if matches!(v, V::T) {
                    "prose"
                } else {
                    "reasoning"
                };
                if open != Some(kind) {
                    parts.extend(open.take());
                    open = Some(kind);
                }
            }
            V::U | V::R => {
                parts.extend(open.take());
                match v {
                    V::U if !pending => {
                        pending = true;
                        parts.push("call");
                    }
                    V::R if pending => {
                        pending = false;
                        parts.push("result");
                    }
                    _ => {}
                }
            }
        }
    }
    parts.extend(open.take());
    parts
}

#[test]
fn committed_part_order_is_the_chunk_run_order_for_every_sequence() {
    let alphabet = [V::T, V::H, V::U, V::R];
    let mut sequences: Vec<Vec<V>> = vec![vec![]];
    let mut checked = 0;
    for _ in 0..5 {
        let mut next = Vec::new();
        for seq in &sequences {
            for v in alphabet {
                let mut s = seq.clone();
                s.push(v);
                next.push(s);
            }
        }
        for seq in &next {
            let mut chunks: Vec<StreamChunk> = seq.iter().copied().map(chunk).collect();
            chunks.push(end(StopReason::EndTurn));
            let (mut state, _) = run(chunks);
            let turn = state.committed_turn.take().expect("committed");
            assert_eq!(kinds(&turn), expected_kinds(seq), "{seq:?}");
            checked += 1;
        }
        sequences = next;
    }
    assert_eq!(checked, 4 + 16 + 64 + 256 + 1024);
}

#[test]
fn duration_spans_first_thinking_chunk_to_flush() {
    let (mut state, clock) = test_reducer_state(1_000);
    let _ = reduce(&mut state, think("a"), &clock);
    clock.advance(Duration::from_millis(1_000));
    let _ = reduce(&mut state, think("b"), &clock);
    clock.advance(Duration::from_millis(3_200));
    let _ = reduce(&mut state, say("x"), &clock);
    let _ = reduce(&mut state, end(StopReason::EndTurn), &clock);
    let turn = state.committed_turn.take().unwrap();
    assert_eq!(
        reasoning_facts(&turn),
        [("ab".to_owned(), Some(4_200), None)],
        "first chunk to flush, not last chunk to flush"
    );
}

#[test]
fn a_second_thinking_run_measures_its_own_duration() {
    let (mut state, clock) = test_reducer_state(0);
    let _ = reduce(&mut state, think("a"), &clock);
    clock.advance(Duration::from_millis(500));
    let _ = reduce(&mut state, say("b"), &clock);
    clock.advance(Duration::from_millis(9_000));
    let _ = reduce(&mut state, think("c"), &clock);
    clock.advance(Duration::from_millis(250));
    let _ = reduce(&mut state, end(StopReason::EndTurn), &clock);
    let turn = state.committed_turn.take().unwrap();
    let durations: Vec<_> = reasoning_facts(&turn).into_iter().map(|f| f.1).collect();
    assert_eq!(durations, [Some(500), Some(250)]);
}

#[test]
fn provider_tokens_attach_to_the_single_reasoning_part() {
    // Still open when the usage arrives: stashed, applied at the flush.
    let (mut state, _) = run(vec![
        think("a"),
        usage(Some(1_300)),
        say("x"),
        end(StopReason::EndTurn),
    ]);
    let turn = state.committed_turn.take().unwrap();
    assert_eq!(reasoning_facts(&turn)[0].2, Some(1_300));
    // Already committed when the usage arrives.
    let (mut state, _) = run(vec![
        think("a"),
        say("x"),
        usage(Some(7)),
        end(StopReason::EndTurn),
    ]);
    assert_eq!(
        reasoning_facts(&state.committed_turn.take().unwrap())[0].2,
        Some(7)
    );
    // No reasoning tokens reported: nothing to attribute.
    let (mut state, _) = run(vec![
        think("a"),
        say("x"),
        usage(None),
        end(StopReason::EndTurn),
    ]);
    assert_eq!(
        reasoning_facts(&state.committed_turn.take().unwrap())[0].2,
        None
    );
}

#[test]
fn two_reasoning_parts_in_one_response_take_no_tokens() {
    for chunks in [
        vec![
            think("a"),
            say("x"),
            think("b"),
            say("y"),
            usage(Some(9)),
            end(StopReason::EndTurn),
        ],
        // One committed, one still open when the usage arrives.
        vec![
            think("a"),
            say("x"),
            think("b"),
            usage(Some(9)),
            end(StopReason::EndTurn),
        ],
    ] {
        let (mut state, _) = run(chunks);
        let turn = state.committed_turn.take().unwrap();
        let facts = reasoning_facts(&turn);
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().all(|f| f.2.is_none()), "{facts:?}");
    }
}

#[test]
fn tokens_belong_to_the_open_response_not_an_earlier_one() {
    let (mut state, _) = run(vec![
        think("first"),
        tool_use("t0"),
        end(StopReason::ToolUse),
        tool_result("t0"),
        think("second"),
        usage(Some(11)),
        say("done"),
        end(StopReason::EndTurn),
    ]);
    let turn = state.committed_turn.take().unwrap();
    let facts = reasoning_facts(&turn);
    assert_eq!(facts.len(), 2);
    assert_eq!(facts[0].2, None, "the earlier response never inherits");
    assert_eq!(facts[1].2, Some(11));
}

#[test]
fn a_usage_after_the_turn_committed_never_mutates_it() {
    let (mut state, clock) = run(vec![think("a"), say("x"), end(StopReason::EndTurn)]);
    let _ = reduce(&mut state, usage(Some(5)), &clock);
    let turn = state.committed_turn.take().unwrap();
    assert_eq!(reasoning_facts(&turn)[0].2, None);
}

#[test]
fn reasoning_serde_is_additive() {
    let bare = TurnPart::Reasoning {
        id: PartId(1),
        text: "x".into(),
        duration_ms: None,
        tokens: None,
    };
    assert_eq!(
        serde_json::to_string(&bare).unwrap(),
        r#"{"kind":"reasoning","id":1,"text":"x"}"#,
        "a part without facts serializes as it always did"
    );
    let loaded: TurnPart =
        serde_json::from_str(r#"{"kind":"reasoning","id":1,"text":"x"}"#).unwrap();
    assert_eq!(loaded, bare, "old JSON loads with no facts");
    let full = TurnPart::Reasoning {
        id: PartId(1),
        text: "x".into(),
        duration_ms: Some(4_200),
        tokens: Some(1_300),
    };
    let json = serde_json::to_string(&full).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"reasoning","id":1,"text":"x","durationMs":4200,"tokens":1300}"#
    );
    assert_eq!(serde_json::from_str::<TurnPart>(&json).unwrap(), full);
}

fn spans_of(parts: &[(&str, &str)]) -> String {
    let mut turn = Turn::new("m".into(), 0);
    for (kind, text) in parts {
        if *kind == "reasoning" {
            turn.push_part(|id| TurnPart::Reasoning {
                id,
                text: (*text).into(),
                duration_ms: None,
                tokens: None,
            });
        } else {
            turn.push_part(|id| TurnPart::Prose {
                id,
                text: (*text).into(),
            });
        }
    }
    turn.responses.push(rustain::domain::models::ResponseMark {
        end: turn.parts.len(),
        call_ids: Vec::new(),
    });
    let mut conversation = Conversation::default();
    conversation.turns.push(turn);
    conversation.rebuild_messages_mirror();
    let message = conversation.messages.last().expect("mirror row");
    let spans: Vec<_> = message
        .content_blocks
        .iter()
        .filter_map(|block| match block {
            rustain::domain::models::ContentBlockType::ResponseSpan(span) => Some(span),
            _ => None,
        })
        .collect();
    format!("{spans:?}")
}

#[test]
fn replayed_provider_span_is_identical_for_either_part_order() {
    let new_order = spans_of(&[("reasoning", "plan"), ("prose", "answer")]);
    let old_order = spans_of(&[("prose", "answer"), ("reasoning", "plan")]);
    assert!(new_order.contains("plan") && new_order.contains("answer"));
    assert_eq!(
        new_order, old_order,
        "provider bytes do not depend on order"
    );
}
