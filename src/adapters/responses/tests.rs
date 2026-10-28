//! Responses behavior vectors for the oh-my-pi wire translation.
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use super::{
    Dialect, WireProfile, events, events::EventMapper, repair, request::ResponsesRequest, schema,
};
use crate::domain::models::{
    CompletionOptions, ImageAttachment, Message, MessageRole, StopReason, StreamChunk,
    ToolDefinition, ToolResultMessage, ToolUseMessage,
};
use serde_json::{Value, json};

fn message(role: MessageRole, content: &str) -> Message {
    Message {
        role,
        content: content.into(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
        provider_replay: vec![],
    }
}

struct Profile(Value);
impl WireProfile for Profile {
    fn dialect(&self) -> Dialect {
        Dialect::Codex
    }
    fn prompt_cache_key(&self) -> Option<&str> {
        Some("conversation")
    }
    fn client_metadata(&self) -> Option<&Value> {
        Some(&self.0)
    }
}

fn options() -> CompletionOptions {
    serde_json::from_value(json!({"model":"gpt-6.1-sol","maxTokens":4096,"systemPrompt":"policy","temperature":0.7,"tools":[]})).unwrap()
}

#[test]
fn request_images_tools_instructions_and_wire_order() {
    let mut user = message(MessageRole::User, "question");
    user.context_prefix = Some("context".into());
    user.images.push(ImageAttachment {
        media_type: "image/png".into(),
        data: "YWJj".into(),
    });
    let mut assistant = message(MessageRole::Assistant, "checking");
    assistant.reasoning_content = Some("never replay this".into());
    assistant.tool_uses.push(ToolUseMessage {
        id: "call_1|item_1".into(),
        name: "read".into(),
        input: json!({"path":"a"}),
    });
    let mut result = message(MessageRole::User, "");
    result.tool_results.push(ToolResultMessage {
        tool_use_id: "call_1|other_item".into(),
        content: "denied".into(),
        is_error: true,
        images: Vec::new(),
    });
    let mut options = options();
    options.tools.push(ToolDefinition {
        name: "read".into(),
        description: "read file".into(),
        input_schema: json!({"type":"object","oneOf":[{"pattern":"(?=x)x"}]}),
        parallel_safe: true,
    });
    let profile = Profile(json!({"session_id":"conversation"}));
    let request = ResponsesRequest::new(
        &[
            user,
            assistant,
            result,
            message(MessageRole::System, "developer"),
        ],
        &options,
        &profile,
    );
    let encoded = serde_json::to_string(&request).unwrap();
    let expected_input = json!([
        {"type":"message","role":"developer","content":[{"type":"input_text","text":"developer"}]},
        {"role":"user","content":[{"type":"input_text","text":"context\n\nquestion"},{"type":"input_image","detail":"auto","image_url":"data:image/png;base64,YWJj"}]},
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"checking","annotations":[]}]},
        {"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a\"}"},
        {"type":"function_call_output","call_id":"call_1","output":"Error: denied"}
    ]);
    let value: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(
        value,
        json!({"model":"gpt-6.1-sol","input":expected_input,"stream":true,"prompt_cache_key":"conversation","tools":[{"type":"function","name":"read","description":"read file","parameters":{"type":"object","anyOf":[true],"properties":{}}}],"instructions":"policy","store":false,"include":["reasoning.encrypted_content"],"client_metadata":{"session_id":"conversation"}})
    );
    let positions: Vec<_> = [
        "\"model\":",
        "\"input\":",
        "\"stream\":",
        "\"prompt_cache_key\":",
        "\"tools\":",
        "\"instructions\":",
        "\"store\":",
        "\"include\":",
        "\"client_metadata\":",
    ]
    .into_iter()
    .map(|key| encoded.find(key).unwrap())
    .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn request_instruction_only_fallback_and_blank_content() {
    let base_options = options();
    let profile = Profile(json!({}));
    let request = ResponsesRequest::new(
        &[
            message(MessageRole::User, " \n"),
            message(MessageRole::System, "last developer"),
        ],
        &base_options,
        &profile,
    );
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(
        value["input"],
        json!([
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"last developer"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"last developer"}]}
        ])
    );
    assert!(value.get("tools").is_none());
    struct Bare;
    impl WireProfile for Bare {
        fn dialect(&self) -> Dialect {
            Dialect::Codex
        }
        fn prompt_cache_key(&self) -> Option<&str> {
            None
        }
        fn client_metadata(&self) -> Option<&Value> {
            None
        }
    }
    let mut options = options();
    options.system_prompt = "  ".into();
    let value = serde_json::to_value(ResponsesRequest::new(&[], &options, &Bare)).unwrap();
    for key in [
        "instructions",
        "tools",
        "client_metadata",
        "prompt_cache_key",
    ] {
        assert!(value.get(key).is_none(), "{key}");
    }
    assert_eq!(value["input"], json!([]));
}

#[test]
fn schema_selective_recursion_and_union_table() {
    let cases = [
        (json!({}), json!(true)),
        (
            json!({"type":["object","null"]}),
            json!({"type":["object","null"],"properties":{}}),
        ),
        (
            json!({"oneOf":[{}, {"type":"object"}],"anyOf":[{"type":"string"}]}),
            json!({"anyOf":[{"type":"string"},true,{"type":"object","properties":{}}]}),
        ),
        (json!({"oneOf":"malformed"}), json!({"oneOf":"malformed"})),
        (json!({"pattern":"(?<=x)y"}), json!(true)),
        (json!({"pattern":"\\(?=x)"}), json!({"pattern":"\\(?=x)"})),
        (
            json!({"pattern":"(?<name>x)"}),
            json!({"pattern":"(?<name>x)"}),
        ),
        (
            json!({"default":{"oneOf":[{}]},"enum":[{}],"const":{},"examples":[{}],"unknown":{"type":"object"}}),
            json!({"default":{"oneOf":[{}]},"enum":[{}],"const":{},"examples":[{}],"unknown":{"type":"object"}}),
        ),
        (
            json!({"items":[{}],"additionalProperties":false}),
            json!({"items":[{}],"additionalProperties":false}),
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(schema::sanitize(input.clone()), expected, "input={input}");
    }
    for key in [
        "properties",
        "patternProperties",
        "dependencies",
        "dependentSchemas",
        "$defs",
        "definitions",
    ] {
        assert_eq!(
            schema::sanitize(json!({key:{"a":{},"b":["literal"]}})),
            json!({key:{"a":true,"b":["literal"]}})
        );
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        let target = if key == "oneOf" { "anyOf" } else { key };
        assert_eq!(schema::sanitize(json!({key:[{}]})), json!({target:[true]}));
    }
    for key in [
        "items",
        "additionalItems",
        "contains",
        "contentSchema",
        "propertyNames",
        "if",
        "then",
        "else",
        "not",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
    ] {
        assert_eq!(schema::sanitize(json!({key:{}})), json!({key:true}));
    }
}

#[test]
fn schema_pattern_fallback_merges_and_preserves_supported_patterns() {
    assert_eq!(
        schema::sanitize(
            json!({"patternProperties":{"(?=a)a":{"type":"string"},"(?=b)b":{"type":"number"},"^ok$":{}}})
        ),
        json!({"patternProperties":{".*":{"anyOf":[{"type":"string"},{"type":"number"}]},"^ok$":true}})
    );
    assert_eq!(
        schema::sanitize(
            json!({"patternProperties":{"(?=a)a":{"anyOf":[{"type":"boolean"}]},"(?=b)b":{},"(?=c)c":{}}})
        ),
        json!({"patternProperties":{".*":{"anyOf":[{"type":"boolean"},true,true]}}})
    );
    // serde's sorted map visits the literal .* last. Like upstream's ordinary
    // property assignment, it replaces the earlier generated fallback.
    assert_eq!(
        schema::sanitize(json!({"patternProperties":{".*":{"type":"boolean"},"(?=x)x":{}}})),
        json!({"patternProperties":{".*":{"type":"boolean"}}})
    );
}

#[test]
fn bun_hash_and_call_id_vectors_are_stable_and_bounded() {
    for (input, hash) in [
        ("", "27k1wwwhf13t"),
        ("empty", "2qpgyfjaphmay"),
        ("abc", "1g45uqqks6lu"),
        ("call abc", "2i3wzu9m4z0vp"),
        ("🦀", "o6v4vva0jzn3"),
    ] {
        assert_eq!(repair::stable_hash(input), hash);
    }
    assert_eq!(repair::stable_hash(&"session".repeat(20)), "3ns620kxzgqs0");
    assert_eq!(repair::sanitize_call_id(""), "call_2qpgyfjaphmay");
    assert_eq!(
        repair::sanitize_call_id("call abc"),
        "call_abc_2i3wzu9m4z0vp"
    );
    assert_eq!(repair::sanitize_call_id("good-id_01|item_a"), "good-id_01");
    assert_eq!(repair::sanitize_call_id("good-id_01\nitem_b"), "good-id_01");
    for input in ["a".repeat(100), "🦀".repeat(70), "___".into(), "a/b".into()] {
        let result = repair::sanitize_call_id(&input);
        assert!(result.len() <= 64 && !result.is_empty());
        assert!(
            result
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        );
        assert_eq!(result, repair::sanitize_call_id(&input));
    }
    assert_ne!(
        repair::sanitize_call_id("a/b"),
        repair::sanitize_call_id("a b")
    );
}

#[test]
fn repair_pairs_orphans_item_ids_and_utf16_truncation() {
    let repaired = repair::repair(
        vec![
            json!({"type":"item_reference","id":"drop"}),
            json!({"type":"function_call","id":"strip","call_id":"missing|first","name":"read","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"orphan","name":"read","output":"🦀".repeat(8001)}),
            json!({"type":"computer_call","id":"keep","call_id":"computer"}),
            json!({"type":"message","id":null,"content":"keep null"}),
        ],
        Dialect::Codex,
    );
    assert_eq!(
        repaired[0],
        json!({"type":"function_call","call_id":"missing","name":"read","arguments":"{}"})
    );
    assert_eq!(
        repaired[1],
        json!({"type":"function_call_output","call_id":"missing","output":"[No tool output recorded: the tool call was interrupted before it produced a result.]"})
    );
    assert_eq!(
        repaired[2]["content"],
        format!(
            "[Previous read result; call_id=orphan]: {}\n...[truncated]",
            "🦀".repeat(8000)
        )
    );
    assert_eq!(
        repaired[3]["content"],
        "[Computer call interrupted before a screenshot was recorded; call_id=computer]"
    );
    assert!(repaired[4]["id"].is_null());
    let paired = repair::repair(
        vec![
            json!({"type":"function_call","call_id":"same|a"}),
            json!({"type":"function_call_output","call_id":"same|b","output":"ok"}),
        ],
        Dialect::Codex,
    );
    assert_eq!(paired.len(), 2);
    assert_eq!(paired[0]["call_id"], paired[1]["call_id"]);
    let split_surrogate = repair::repair(
        vec![
            json!({"type":"function_call_output","call_id":"x","output":format!("{}🦀", "a".repeat(15999))}),
        ],
        Dialect::Codex,
    );
    assert_eq!(
        split_surrogate[0]["content"],
        format!(
            "[Previous tool result; call_id=x]: {}\n...[truncated]",
            "a".repeat(15999)
        )
    );
}

#[test]
fn repair_preserves_computer_ids_and_matches_exchange_kinds() {
    let input = vec![
        json!({"type":"computer_call","id":"preserved","call_id":"computer"}),
        json!({"type":"computer_call_output","call_id":"computer","output":"screenshot"}),
        json!({"type":"custom_tool_call","id":"removed","call_id":"custom","name":"patch"}),
        json!({"type":"function_call_output","call_id":"custom","output":{"n":1}}),
    ];
    let output = repair::repair(input, Dialect::Codex);
    assert_eq!(output[0]["id"], "preserved");
    assert_eq!(output[1]["type"], "computer_call_output");
    assert!(output[2].get("id").is_none());
    assert_eq!(output[3]["type"], "custom_tool_call_output");
    assert_eq!(
        output[3]["output"],
        "[No tool output recorded: the tool call was interrupted before it produced a result.]"
    );
    assert_eq!(
        output[4]["content"],
        "[Previous tool result; call_id=custom]: {\"n\":1}"
    );
}

fn feed(mapper: &mut EventMapper, event: Value) -> Vec<StreamChunk> {
    mapper.event(event).unwrap()
}

#[test]
fn events_interleaved_final_arguments_are_authoritative_and_emit_once() {
    let mut mapper = EventMapper::new();
    for (index, id, name) in [(0, "a", "read"), (1, "b", "write")] {
        assert!(feed(&mut mapper, json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","call_id":id,"name":name}})).is_empty());
    }
    for (index, delta) in [(0, "{\"x\":"), (1, "{\"y\":"), (0, "1}"), (1, "2}")] {
        assert!(feed(&mut mapper, json!({"type":"response.function_call_arguments.delta","output_index":index,"delta":delta})).is_empty());
    }
    assert!(!mapper.committed());
    assert!(feed(&mut mapper, json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"x\":3}"})).is_empty());
    let chunks = feed(
        &mut mapper,
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"a","name":"read","arguments":"{\"x\":4}"}}),
    );
    assert!(
        matches!(&chunks[..], [StreamChunk::ToolUse {id,name,input}] if id=="a" && name=="read" && input==&json!({"x":4}))
    );
    assert!(mapper.committed());
    assert!(feed(&mut mapper, json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"a","name":"read","arguments":"{}"}})).is_empty());
    let chunks = feed(
        &mut mapper,
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"b","name":"write","arguments":"{\"y\":5}"}}),
    );
    assert!(matches!(&chunks[..], [StreamChunk::ToolUse {input,..}] if input==&json!({"y":5})));
    assert!(feed(&mut mapper, json!({"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"hidden"}})).is_empty());
    let chunks = feed(
        &mut mapper,
        json!({"type":"response.completed","response":{"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40,"cache_write_tokens":10},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":5}}}}),
    );
    assert!(
        matches!(&chunks[0], StreamChunk::Usage {usage,..} if usage.input_tokens==50 && usage.cache_read_input_tokens==Some(40) && usage.cache_creation_input_tokens==Some(10) && usage.output_tokens==7 && usage.reasoning_tokens==Some(5))
    );
    assert!(matches!(
        &chunks[1],
        StreamChunk::TurnComplete {
            stop_reason: StopReason::ToolUse
        }
    ));
    assert!(mapper.terminal());
    assert!(feed(&mut mapper, json!({"type":"response.done"})).is_empty());
}

#[test]
fn events_item_id_routing_terminal_fallback_and_malformed_json() {
    let mut mapper = EventMapper::new();
    for (index, id) in [(0, "item_a"), (1, "item_b")] {
        feed(
            &mut mapper,
            json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","id":id,"call_id":id,"name":"tool"}}),
        );
    }
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.done","item_id":"item_a","arguments":"{\"done\":true}"}),
    );
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.done","item_id":"item_b","arguments":"bad"}),
    );
    let chunks = feed(
        &mut mapper,
        json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}),
    );
    assert!(
        matches!(&chunks[0],StreamChunk::ToolUse {id,input,..} if id=="item_a" && input==&json!({"done":true}))
    );
    assert!(
        matches!(&chunks[1],StreamChunk::ToolUse {id,input,..} if id=="item_b" && input==&json!({}))
    );
    assert!(matches!(
        &chunks[2],
        StreamChunk::TurnComplete {
            stop_reason: StopReason::ToolUse
        }
    ));
}

#[test]
fn events_item_id_wins_late_deltas_do_not_leak_and_empty_final_falls_back() {
    let mut mapper = EventMapper::new();
    for (index, id) in [(0, "a"), (1, "b")] {
        feed(
            &mut mapper,
            json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","id":id,"call_id":id,"name":"tool"}}),
        );
    }
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.delta","item_id":"a","output_index":1,"delta":"{\"a\":1}"}),
    );
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.delta","item_id":"b","output_index":0,"delta":"{\"b\":2}"}),
    );
    let first = feed(
        &mut mapper,
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"a","name":"tool","arguments":""}}),
    );
    assert!(
        matches!(&first[..], [StreamChunk::ToolUse {id,input,..}] if id=="a" && input==&json!({"a":1}))
    );
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.delta","item_id":"a","delta":"junk"}),
    );
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.done","item_id":"unknown","output_index":1,"arguments":"{}"}),
    );
    let second = feed(
        &mut mapper,
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"b","call_id":"b","name":"tool","arguments":""}}),
    );
    assert!(
        matches!(&second[..], [StreamChunk::ToolUse {id,input,..}] if id=="b" && input==&json!({"b":2}))
    );
}

#[test]
fn events_unfinished_arguments_do_not_execute_and_incomplete_tools_keep_stop_reason() {
    let mut mapper = EventMapper::new();
    feed(
        &mut mapper,
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"unfinished","name":"tool"}}),
    );
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"partial\":"}),
    );
    let chunks = feed(&mut mapper, json!({"type":"response.completed"}));
    assert!(matches!(
        &chunks[..],
        [StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn
        }]
    ));
    assert!(!mapper.committed());
    let mut mapper = EventMapper::new();
    feed(
        &mut mapper,
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"finished","name":"tool","arguments":"{}"}}),
    );
    let chunks = feed(
        &mut mapper,
        json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}),
    );
    assert!(matches!(
        &chunks[..],
        [StreamChunk::TurnComplete {
            stop_reason: StopReason::MaxTokens
        }]
    ));
}

#[test]
fn events_text_refusal_thinking_stop_and_usage_floor() {
    for (kind, thinking) in [
        ("response.output_text.delta", false),
        ("response.refusal.delta", false),
        ("response.reasoning_summary_text.delta", true),
        ("response.reasoning_text.delta", true),
    ] {
        let mut mapper = EventMapper::new();
        let chunks = feed(&mut mapper, json!({"type":kind,"delta":"hello 🦀"}));
        assert!(
            matches!(&chunks[0],StreamChunk::Thinking {content,..} if thinking && content=="hello 🦀")
                || matches!(&chunks[0],StreamChunk::Text {content,..} if !thinking && content=="hello 🦀")
        );
        assert!(mapper.committed());
    }
    for (reason, expected) in [
        ("max_output_tokens", StopReason::MaxTokens),
        ("content_filter", StopReason::MaxTokens),
        ("steered", StopReason::EndTurn),
    ] {
        let chunks = feed(
            &mut EventMapper::new(),
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":reason},"usage":{"input_tokens":1,"input_tokens_details":{"cached_tokens":10},"output_tokens":2}}}),
        );
        assert!(matches!(&chunks[0],StreamChunk::Usage {usage,..} if usage.input_tokens==0));
        assert!(
            matches!(&chunks[1],StreamChunk::TurnComplete {stop_reason} if stop_reason==&expected)
        );
    }
}

#[test]
fn events_whitespace_guard_event_and_byte_caps_without_tool_commit() {
    let mut mapper = EventMapper::new();
    feed(
        &mut mapper,
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item","call_id":"call","name":"tool"}}),
    );
    for _ in 0..255 {
        assert!(feed(&mut mapper,json!({"type":"response.function_call_arguments.delta","item_id":"item","delta":" "})).is_empty());
    }
    let error = mapper
        .event(
            json!({"type":"response.function_call_arguments.delta","item_id":"item","delta":" "}),
        )
        .unwrap_err();
    assert!(error.whitespace_loop && error.retryable && !mapper.committed());
    let mut mapper = EventMapper::new();
    let error = mapper.event(json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":" ".repeat(16384)})).unwrap_err();
    assert!(error.whitespace_loop && !mapper.committed());
    let mut mapper = EventMapper::new();
    for _ in 0..255 {
        feed(
            &mut mapper,
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":" "}),
        );
    }
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"x"}),
    );
    feed(
        &mut mapper,
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":" "}),
    );
    assert!(!mapper.committed());
}

#[test]
fn events_failure_retry_classification_and_metadata_silence() {
    for (event, retryable) in [
        (
            json!({"type":"response.failed","response":{"error":{"code":"SERVER_ERROR","message":"bad"}}}),
            true,
        ),
        (
            json!({"type":"error","error":{"message":"Service unavailable"}}),
            true,
        ),
        (
            json!({"type":"response.failed","message":"peer closed connection without sending complete message body (incomplete chunked read)"}),
            true,
        ),
        (
            json!({"type":"error","message":"<StreamReset stream_id:13, error_code:7, remote_reset:True>"}),
            true,
        ),
        (
            json!({"type":"error","message":"<StreamReset stream_id:13, error_code:9, remote_reset:True>"}),
            false,
        ),
        (
            json!({"type":"response.failed","error":{"code":"invalid_request_error","message":"invalid field"}}),
            false,
        ),
    ] {
        let error = EventMapper::new().event(event).unwrap_err();
        assert_eq!(error.retryable, retryable, "{}", error.message);
        assert!(!error.whitespace_loop);
    }
    let mut mapper = EventMapper::new();
    assert!(
        feed(
            &mut mapper,
            json!({"type":"response.metadata","headers":{"x-models-etag":"e"}})
        )
        .is_empty()
    );
    assert!(!mapper.committed() && !mapper.terminal());
}

#[test]
fn events_failed_or_cancelled_terminal_status_is_an_error_not_a_turn() {
    for status in ["failed", "cancelled"] {
        let mut mapper = EventMapper::new();
        feed(
            &mut mapper,
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call","name":"tool","arguments":"{}"}}),
        );
        let error = mapper
            .event(json!({"type":"response.completed","response":{"status":status}}))
            .unwrap_err();
        assert!(error.message.contains(&format!("status={status}")));
        assert!(!mapper.terminal());
    }
}

#[test]
fn events_calls_without_ids_are_not_deduplicated_together() {
    let mut mapper = EventMapper::new();
    let mut tools = 0;
    for index in 0..2 {
        tools += feed(
            &mut mapper,
            json!({"type":"response.output_item.done","output_index":index,"item":{"type":"function_call","name":"tool","arguments":"{}"}}),
        )
        .iter()
        .filter(|chunk| matches!(chunk, StreamChunk::ToolUse { .. }))
        .count();
    }
    assert_eq!(tools, 2);
}

#[test]
fn prompt_cache_key_counts_utf16_and_uses_reference_hash() {
    assert_eq!(super::prompt_cache_key("short"), "short");
    assert!(super::prompt_cache_key(&"🦀".repeat(33)).starts_with("pc_"));
    assert_eq!(super::prompt_cache_key(&"🦀".repeat(32)), "🦀".repeat(32));
    assert!(super::prompt_cache_key(&"a".repeat(65)).len() < 64);
}

struct Generic {
    reasoning: bool,
    vision: bool,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
    metadata: Value,
}
impl WireProfile for Generic {
    fn dialect(&self) -> Dialect {
        Dialect::Generic
    }
    fn prompt_cache_key(&self) -> Option<&str> {
        Some("conversation")
    }
    fn client_metadata(&self) -> Option<&Value> {
        Some(&self.metadata)
    }
    fn replay_provider(&self) -> Option<&str> {
        Some("xai")
    }
    fn reasoning(&self) -> bool {
        self.reasoning
    }
    fn vision(&self) -> bool {
        self.vision
    }
    fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }
    fn temperature(&self) -> Option<f32> {
        self.temperature
    }
    fn reject_root_object_union(&self) -> bool {
        true
    }
}

fn generic(reasoning: bool, vision: bool, sampling: bool) -> Generic {
    Generic {
        reasoning,
        vision,
        max_output_tokens: sampling.then_some(8192),
        temperature: sampling.then_some(0.5),
        metadata: json!({"never":"sent"}),
    }
}

fn tool(name: &str, input_schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("{name} tool"),
        input_schema,
        parallel_safe: true,
    }
}

fn replay(model: &str, provider: &str, marker: &str) -> crate::domain::models::ProviderReplayItem {
    crate::domain::models::ProviderReplayItem {
        provider: provider.into(),
        model: model.into(),
        payload: std::sync::Arc::new(json!({"type":"reasoning","encrypted_content":marker})),
    }
}

#[test]
fn generic_request_reasoning_literal_and_wire_order() {
    let mut user = message(MessageRole::User, "question");
    user.images.push(ImageAttachment {
        media_type: "image/png".into(),
        data: "YWJj".into(),
    });
    let mut assistant = message(MessageRole::Assistant, "answer");
    assistant.provider_replay = vec![
        replay("grok-4.6", "xai", "keep"),
        replay("grok-4.3", "xai", "other model"),
        replay("grok-4.6", "openai-codex", "other provider"),
    ];
    assistant.tool_uses.push(ToolUseMessage {
        id: "call_1".into(),
        name: "read".into(),
        input: json!({"path":"a"}),
    });
    let mut result = message(MessageRole::User, "");
    result.tool_results.push(ToolResultMessage {
        tool_use_id: "call_1".into(),
        content: "ok".into(),
        is_error: false,
        images: Vec::new(),
    });
    let mut options = options();
    options.model = "grok-4.6".into();
    options.tools = vec![
        tool(
            "read",
            json!({"type":"object","properties":{"path":{"type":"string"}},"anyOf":[{"required":["path"]},{"required":["glob"],"description":"d"}]}),
        ),
        tool(
            "contradiction",
            json!({"type":"object","properties":{"mode":{"type":"string","enum":["a",1]}}}),
        ),
        tool(
            "union",
            json!({"type":"object","properties":{},"anyOf":[{"type":"string"}]}),
        ),
    ];
    let profile = generic(true, true, true);
    let encoded = serde_json::to_string(&ResponsesRequest::new(
        &[
            message(MessageRole::System, "history policy"),
            user,
            assistant,
            result,
            message(MessageRole::System, "  "),
        ],
        &options,
        &profile,
    ))
    .unwrap();
    let value: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(
        value,
        json!({
            "model":"grok-4.6",
            "input":[
                {"role":"user","content":[{"type":"input_text","text":"question"},{"type":"input_image","detail":"auto","image_url":"data:image/png;base64,YWJj"}]},
                {"type":"reasoning","encrypted_content":"keep"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]},
                {"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ],
            "instructions":"policy\n\nhistory policy",
            "stream":true,
            "prompt_cache_key":"conversation",
            "store":false,
            "max_output_tokens":8192,
            "temperature":0.5,
            "tools":[{"type":"function","name":"read","description":"read tool","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],
            "include":["reasoning.encrypted_content"]
        })
    );
    let positions: Vec<_> = [
        "\"model\":",
        "\"input\":",
        "\"instructions\":",
        "\"stream\":",
        "\"prompt_cache_key\":",
        "\"store\":",
        "\"max_output_tokens\":",
        "\"temperature\":",
        "\"tools\":",
        "\"include\":",
    ]
    .into_iter()
    .map(|key| encoded.find(key).unwrap())
    .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn generic_request_non_reasoning_non_vision_and_no_synthetic_user_item() {
    let mut look = message(MessageRole::User, "look");
    let mut only_image = message(MessageRole::User, " ");
    for _ in 0..2 {
        let image = ImageAttachment {
            media_type: "image/png".into(),
            data: "YWJj".into(),
        };
        look.images.push(image.clone());
        only_image.images.push(image);
    }
    let mut options = options();
    options.model = "grok-composer-2.5-fast".into();
    let profile = generic(false, false, false);
    let value = serde_json::to_value(ResponsesRequest::new(
        &[look, only_image],
        &options,
        &profile,
    ))
    .unwrap();
    let placeholder =
        json!({"type":"input_text","text":"[image omitted: model does not support vision]"});
    assert_eq!(
        value,
        json!({
            "model":"grok-composer-2.5-fast",
            "input":[
                {"role":"user","content":[{"type":"input_text","text":"look"},placeholder]},
                {"role":"user","content":[placeholder]}
            ],
            "instructions":"policy",
            "stream":true,
            "prompt_cache_key":"conversation",
            "store":false
        })
    );
    options.system_prompt = String::new();
    let value = serde_json::to_value(ResponsesRequest::new(
        &[message(MessageRole::System, "only system")],
        &options,
        &profile,
    ))
    .unwrap();
    assert_eq!(value["input"], json!([]));
    assert_eq!(value["instructions"], "only system");
    let value = serde_json::to_value(ResponsesRequest::new(&[], &options, &profile)).unwrap();
    assert!(value.get("instructions").is_none());
}

#[test]
fn generic_repair_orders_orphans_and_codex_keeps_unordered_pairing() {
    let input = || {
        vec![
            json!({"type":"function_call_output","call_id":"early","output":null}),
            json!({"type":"function_call","call_id":"early","name":"x","arguments":"{}"}),
            json!({"type":"computer_call_output","call_id":"cmp","output":{"k":1}}),
        ]
    };
    assert_eq!(
        repair::repair(input(), Dialect::Generic),
        vec![
            json!({"type":"message","role":"assistant","content":"[Orphan tool result; call_id=early]: "}),
            json!({"type":"function_call","call_id":"early","name":"x","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"early","output":repair::CODEX_INTERRUPTED_TOOL_OUTPUT}),
            json!({"type":"message","role":"assistant","content":"[Orphan computer result; call_id=cmp]: {\"k\":1}"}),
        ]
    );
    let codex = repair::repair(input(), Dialect::Codex);
    assert_eq!(codex[0]["type"], "function_call_output");
    assert_eq!(codex[1]["type"], "function_call");
    assert_eq!(
        codex[2]["content"],
        "[Previous tool result; call_id=cmp]: {\"k\":1}"
    );
}

#[test]
fn generic_hoist_moves_wedged_messages_ahead_of_the_batch_only() {
    let call = |id: &str| json!({"type":"function_call","call_id":id,"name":"t","arguments":"{}"});
    let output = |id: &str| json!({"type":"function_call_output","call_id":id,"output":"ok"});
    let note = |text: &str| json!({"type":"message","role":"assistant","content":text});
    let wedged = vec![call("a"), note("m"), call("b"), output("a"), output("b")];
    let hoisted = repair::hoist(wedged);
    assert_eq!(
        hoisted,
        vec![note("m"), call("a"), call("b"), output("a"), output("b")]
    );
    assert_eq!(repair::hoist(hoisted.clone()), hoisted);
    let rounds = vec![call("a"), output("a"), note("m"), call("b"), output("b")];
    assert_eq!(repair::hoist(rounds.clone()), rounds);
    let user = json!({"role":"user","content":"u"});
    let fenced = vec![call("a"), user.clone(), note("m"), output("a")];
    assert_eq!(repair::hoist(fenced.clone()), fenced);
}

#[test]
fn schema_root_union_flatten_and_strict_quarantine_table() {
    for (input, expected) in [
        (
            json!({"type":"object","properties":{},"anyOf":[{"required":["a"]},{"required":["b"],"title":"t"}]}),
            json!({"type":"object","properties":{}}),
        ),
        (
            json!({"properties":{},"oneOf":[{"required":["a"],"description":"d"}]}),
            json!({"properties":{}}),
        ),
        (
            json!({"type":["object","null"],"anyOf":[{"required":["a"]}]}),
            json!({"type":["object","null"]}),
        ),
        (
            json!({"type":"object","anyOf":[{"type":"object","required":["a"]}]}),
            json!({"type":"object","anyOf":[{"type":"object","required":["a"]}]}),
        ),
        (
            json!({"anyOf":[{"required":["a"]}]}),
            json!({"anyOf":[{"required":["a"]}]}),
        ),
        (
            json!({"type":"object","anyOf":[{"required":[]}]}),
            json!({"type":"object","anyOf":[{"required":[]}]}),
        ),
        (
            json!({"type":"object","anyOf":[{"required":["a"],"minProperties":1}]}),
            json!({"type":"object","anyOf":[{"required":["a"],"minProperties":1}]}),
        ),
        (
            json!({"type":"object","anyOf":[]}),
            json!({"type":"object","anyOf":[]}),
        ),
        (json!(true), json!(true)),
    ] {
        assert_eq!(
            schema::flatten_exclusive_required_root_union(input.clone()),
            expected,
            "input={input}"
        );
    }
    for (input, reject, expected) in [
        (
            json!({"type":"string","enum":["a",1]}),
            false,
            Some("#/enum"),
        ),
        (
            json!({"type":"integer","const":1.5}),
            false,
            Some("#/const"),
        ),
        (json!({"type":"integer","const":2.0}), false, None),
        (
            json!({"type":["integer","null"],"enum":[1,null]}),
            false,
            None,
        ),
        (json!({"type":"unknown","enum":[1]}), false, None),
        (
            json!({"type":"object","properties":{"x":{"type":"null","enum":["no"]}}}),
            false,
            Some("#/properties/x/enum"),
        ),
        (
            json!({"items":[{"type":"boolean"},{"type":"boolean","const":"x"}]}),
            false,
            Some("#/items/1/const"),
        ),
        (
            json!({"allOf":[{"type":"array","enum":[[]]},{"type":"array","enum":[{}]}]}),
            false,
            Some("#/allOf/1/enum"),
        ),
        (
            json!({"type":"object","anyOf":[{"type":"string"}]}),
            true,
            Some("#/anyOf"),
        ),
        (
            json!({"type":"object","anyOf":[{"type":"string"}]}),
            false,
            None,
        ),
        (
            json!({"properties":{},"oneOf":[true]}),
            true,
            Some("#/oneOf"),
        ),
        (
            json!({"properties":null,"anyOf":[{}]}),
            true,
            Some("#/anyOf"),
        ),
        (
            json!({"properties":{},"oneOf":[{"type":"object"}]}),
            true,
            None,
        ),
        (json!({"anyOf":[{"type":"string"}]}), true, None),
        (
            json!({"type":"object","properties":{"p":{"type":"object","anyOf":[{"type":"string"}]}}}),
            true,
            None,
        ),
    ] {
        assert_eq!(
            schema::strict_violation(&input, reject).as_deref(),
            expected,
            "input={input} reject={reject}"
        );
    }
}

#[test]
fn replay_unsafe_event_table() {
    for (event, unsafe_event) in [
        (
            json!({"type":"response.output_text.delta","delta":"x"}),
            true,
        ),
        (
            json!({"type":"response.output_text.delta","delta":""}),
            false,
        ),
        (json!({"type":"response.refusal.delta","delta":"x"}), true),
        (
            json!({"type":"response.reasoning_summary_text.delta","delta":"x"}),
            true,
        ),
        (
            json!({"type":"response.reasoning_text.delta","delta":"x"}),
            true,
        ),
        (
            json!({"type":"response.function_call_arguments.delta","delta":" "}),
            true,
        ),
        (
            json!({"type":"response.function_call_arguments.delta"}),
            false,
        ),
        (
            json!({"type":"response.custom_tool_call_input.delta","delta":"x"}),
            true,
        ),
        (json!({"type":"response.reasoning_summary_part.done"}), true),
        (
            json!({"type":"response.output_item.done","item":{"type":"reasoning"}}),
            true,
        ),
        (
            json!({"type":"response.output_item.added","item":{"type":"message"}}),
            false,
        ),
        (json!({"type":"response.created"}), false),
        (json!({"type":"response.completed"}), false),
    ] {
        assert_eq!(events::replay_unsafe(&event), unsafe_event, "{event}");
    }
}

#[test]
fn generic_event_texts_retry_classes_and_terminal_rules() {
    let generic = || EventMapper::new().with_dialect(Dialect::Generic);
    for (event, message, retryable) in [
        (
            json!({"type":"error","code":"rate_limit_exceeded","message":"Slow down"}),
            "Error Code rate_limit_exceeded: Slow down",
            true,
        ),
        (
            json!({"type":"error","error":{"message":"bad input"}}),
            "Error Code unknown: bad input",
            false,
        ),
        (
            json!({"type":"error"}),
            "Error Code unknown: no message",
            false,
        ),
        (
            json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"boom"}}}),
            "server_error: boom",
            true,
        ),
        (
            json!({"type":"response.failed","response":{"error":{"code":"","message":"You exceeded your usage limit"}}}),
            "unknown: You exceeded your usage limit",
            false,
        ),
        (
            json!({"type":"response.failed","response":{"status_details":{"error":{"code":"overloaded"}}}}),
            "overloaded: no message",
            true,
        ),
        (
            json!({"type":"response.failed","response":{"incomplete_details":{"reason":"max_output_tokens"}}}),
            "incomplete: max_output_tokens",
            false,
        ),
        (
            json!({"type":"response.failed","response":{"status_details":{"reason":"moderation"}}}),
            "Unknown error (no error details in response)",
            true,
        ),
        (
            json!({"type":"response.completed","response":{"status":"failed","status_details":{"reason":"moderation"}}}),
            "status_details: moderation",
            false,
        ),
        (
            json!({"type":"response.done","response":{"status":"cancelled"}}),
            "Unknown error (no error details in response)",
            true,
        ),
        (
            json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}}),
            "incomplete: content_filter",
            false,
        ),
    ] {
        let mut mapper = generic();
        let error = mapper.event(event.clone()).unwrap_err();
        assert_eq!(error.message, message, "{event}");
        assert_eq!(error.retryable, retryable, "{event}");
        assert!(!error.whitespace_loop && !mapper.terminal());
    }
    // `steered` stays Codex-only: a generic incomplete turn is a length stop.
    let chunks = feed(
        &mut generic(),
        json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"steered"}}}),
    );
    assert!(matches!(
        &chunks[..],
        [StreamChunk::TurnComplete {
            stop_reason: StopReason::MaxTokens
        }]
    ));
    // No whitespace-loop guard for the generic dialect.
    let mut mapper = generic();
    for _ in 0..300 {
        assert!(feed(&mut mapper,json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":" "})).is_empty());
    }
}

#[test]
fn usage_orchestration_subtraction_only_when_the_total_includes_it() {
    let usage = |total: Option<u64>| {
        let mut usage = json!({"input_tokens":120,"output_tokens":30,"input_tokens_details":{"cached_tokens":50,"orchestration_input_tokens":20,"orchestration_input_cached_tokens":30},"output_tokens_details":{"orchestration_output_tokens":5}});
        if let Some(total) = total {
            usage["total_tokens"] = json!(total);
        }
        let chunks = feed(
            &mut EventMapper::new().with_dialect(Dialect::Generic),
            json!({"type":"response.completed","response":{"usage":usage}}),
        );
        match &chunks[0] {
            StreamChunk::Usage { usage, .. } => (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
            ),
            other => panic!("{other:?}"),
        }
    };
    // Total 150 equals the primary counts: orchestration (20 in, 20 cached, 5 out) is inside them.
    assert_eq!(usage(Some(150)), (70, 25, Some(30)));
    // Total 175 reports orchestration separately: the primary counts stand.
    assert_eq!(usage(Some(175)), (70, 30, Some(50)));
    assert_eq!(usage(None), (70, 30, Some(50)));
}
