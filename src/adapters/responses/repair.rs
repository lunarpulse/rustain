//! Responses history repair, adapted from oh-my-pi request-transformer.ts and,
//! for the generic dialect, openai-shared.ts (orphan repair and batch hoist).
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
//!
//! The one-shot Wyhash translation below follows Zig 0.14.1's
//! lib/std/hash/wyhash.zig, the algorithm used by Bun.hash (UTF-8, seed zero).
//!
//! The MIT License (Expat)
//!
//! Copyright (c) Zig contributors
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in
//! all copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
//! THE SOFTWARE.

use super::Dialect;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub const CODEX_INTERRUPTED_TOOL_OUTPUT: &str =
    "[No tool output recorded: the tool call was interrupted before it produced a result.]";
const ORPHAN_OUTPUT_LIMIT: usize = 16_000;

/// Bun.hash(text).toString(36), shared with profile prompt-cache identities.
pub fn stable_hash(text: &str) -> String {
    let mut value = wyhash(text.as_bytes());
    let mut digits = [0u8; 13];
    let mut start = digits.len();
    loop {
        start -= 1;
        let digit = (value % 36) as u8;
        digits[start] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        value /= 36;
        if value == 0 {
            break;
        }
    }
    String::from_utf8(digits[start..].to_vec()).expect("base36 is ASCII")
}

/// Canonicalize composite call/item IDs, preserving collision resistance even
/// for short lossy changes. Hash the call half, not the provider's item half.
pub fn sanitize_call_id(raw: &str) -> String {
    if raw.is_empty() {
        return format!("call_{}", stable_hash("empty"));
    }
    let base = match raw.find(['\n', '|']) {
        Some(0) => &raw[1..],
        Some(index) => &raw[..index],
        None => raw,
    };
    let mut sanitized: String = base
        .encode_utf16()
        .map(|unit| {
            if unit <= 127
                && ((unit as u8).is_ascii_alphanumeric()
                    || unit == u16::from(b'_')
                    || unit == u16::from(b'-'))
            {
                unit as u8 as char
            } else {
                '_'
            }
        })
        .collect();
    sanitized.truncate(sanitized.trim_end_matches('_').len());
    if !sanitized.is_empty() && sanitized.len() <= 64 && sanitized == base {
        return sanitized;
    }
    let hash = stable_hash(if base.is_empty() { raw } else { base });
    if sanitized.is_empty() {
        sanitized.push_str("call");
    }
    sanitized.truncate(sanitized.len().min(63 - hash.len()));
    sanitized.push('_');
    sanitized.push_str(&hash);
    sanitized
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Function,
    Custom,
    Computer,
}

fn call_kind(item: &Value) -> Option<Kind> {
    match item["type"].as_str()? {
        "function_call" => Some(Kind::Function),
        "custom_tool_call" => Some(Kind::Custom),
        "computer_call" => Some(Kind::Computer),
        _ => None,
    }
}

fn output_kind(item: &Value) -> Option<Kind> {
    match item["type"].as_str()? {
        "function_call_output" => Some(Kind::Function),
        "custom_tool_call_output" => Some(Kind::Custom),
        "computer_call_output" => Some(Kind::Computer),
        _ => None,
    }
}

enum Action {
    Keep,
    Orphan(Kind),
    Interrupted(Kind),
}

/// Filter references and stored item IDs, normalize IDs, then repair both halves
/// of tool exchanges. Values move into the repaired array without tree copies.
///
/// `Codex` pairs a call with an output anywhere in the input; `Generic` follows
/// omp's ordered rules (an output needs an earlier call, a call a later output).
pub fn repair(mut input: Vec<Value>, dialect: Dialect) -> Vec<Value> {
    input.retain(|item| item["type"] != "item_reference");
    for item in &mut input {
        if let Some(object) = item.as_object_mut() {
            if object.get("type").and_then(Value::as_str) != Some("computer_call")
                && object.get("id").is_some_and(|id| !id.is_null())
            {
                object.remove("id");
            }
            if let Some(Value::String(id)) = object.get_mut("call_id") {
                *id = sanitize_call_id(id);
            }
        }
    }
    let actions = match dialect {
        Dialect::Codex => unordered_actions(&input),
        Dialect::Generic => ordered_actions(&input),
    };
    let mut repaired = Vec::with_capacity(input.len());
    for (mut item, action) in input.into_iter().zip(actions) {
        match action {
            Action::Keep => repaired.push(item),
            Action::Orphan(kind) => {
                let id = item["call_id"].as_str().expect("orphan ID");
                let note = match dialect {
                    Dialect::Codex => {
                        let name = item["name"].as_str().unwrap_or("tool");
                        let output = if let Some(text) = item["output"].as_str() {
                            truncate_output(text)
                        } else {
                            truncate_output(&item["output"].to_string())
                        };
                        format!("[Previous {name} result; call_id={id}]: {output}")
                    }
                    Dialect::Generic => {
                        let name = if kind == Kind::Computer {
                            "computer"
                        } else {
                            "tool"
                        };
                        let output = match &item["output"] {
                            Value::String(text) => truncate_output(text),
                            Value::Null => String::new(),
                            other => truncate_output(&other.to_string()),
                        };
                        format!("[Orphan {name} result; call_id={id}]: {output}")
                    }
                };
                repaired.push(json!({"type":"message","role":"assistant","content":note}));
            }
            Action::Interrupted(Kind::Computer) => {
                let id = item["call_id"].as_str().expect("computer ID");
                repaired.push(json!({"type":"message","role":"assistant","content":format!("[Computer call interrupted before a screenshot was recorded; call_id={id}]")}));
            }
            Action::Interrupted(kind) => {
                let id = item["call_id"].take();
                item["call_id"] = id.clone();
                repaired.push(item);
                repaired.push(json!({"type":if kind == Kind::Custom { "custom_tool_call_output" } else { "function_call_output" },"call_id":id,"output":CODEX_INTERRUPTED_TOOL_OUTPUT}));
            }
        }
    }
    repaired
}

fn unordered_actions(input: &[Value]) -> Vec<Action> {
    let mut calls = HashMap::new();
    let mut outputs = HashMap::new();
    for item in input {
        let Some(id) = item["call_id"].as_str() else {
            continue;
        };
        if let Some(kind) = call_kind(item) {
            calls.insert(id, kind);
        }
        if let Some(kind) = output_kind(item) {
            outputs.insert(id, kind);
        }
    }
    input
        .iter()
        .map(|item| {
            let Some(id) = item["call_id"].as_str() else {
                return Action::Keep;
            };
            if let Some(kind) = output_kind(item) {
                if calls.get(id) != Some(&kind) {
                    return Action::Orphan(kind);
                }
            }
            if let Some(kind) = call_kind(item) {
                if outputs.get(id) != Some(&kind) {
                    return Action::Interrupted(kind);
                }
            }
            Action::Keep
        })
        .collect()
}

/// omp `repairOrphanResponsesToolOutputs` then `repairOrphanResponsesToolCalls`:
/// an output folded into a note no longer pairs a later-checked call.
fn ordered_actions(input: &[Value]) -> Vec<Action> {
    let mut actions: Vec<Action> = Vec::with_capacity(input.len());
    let mut preceding = HashSet::new();
    for item in input {
        let id = item["call_id"].as_str();
        if let (Some(kind), Some(id)) = (call_kind(item), id) {
            preceding.insert((kind, id));
        }
        actions.push(match (output_kind(item), id) {
            (Some(kind), Some(id)) if !preceding.contains(&(kind, id)) => Action::Orphan(kind),
            _ => Action::Keep,
        });
    }
    let mut later = HashSet::new();
    for (item, action) in input.iter().zip(actions.iter_mut()).rev() {
        let Some(id) = item["call_id"].as_str() else {
            continue;
        };
        if let (Some(kind), Action::Keep) = (output_kind(item), &*action) {
            later.insert((kind, id));
        }
        if let Some(kind) = call_kind(item) {
            if !later.contains(&(kind, id)) {
                *action = Action::Interrupted(kind);
            }
        }
    }
    actions
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Batch {
    Call,
    Output,
    AssistantMessage,
    Other,
}

fn batch_kind(item: &Value) -> Batch {
    if call_kind(item).is_some() {
        Batch::Call
    } else if output_kind(item).is_some() {
        Batch::Output
    } else if item["type"] == "message" && item["role"] == "assistant" {
        Batch::AssistantMessage
    } else {
        Batch::Other
    }
}

/// omp `hoistInterleavedResponsesToolBatchMessages`: assistant messages wedged
/// inside a call → output batch move ahead of that batch. Idempotent.
pub fn hoist(items: Vec<Value>) -> Vec<Value> {
    let kinds: Vec<Batch> = items.iter().map(batch_kind).collect();
    let call_id = |index: usize| items[index]["call_id"].as_str();
    // Does a call with `id` precede `index` within the same contiguous batch body?
    let earlier_batch_call = |index: usize, id: &str| {
        for probe in (0..index).rev() {
            match kinds[probe] {
                Batch::Other => return false,
                Batch::Call if call_id(probe) == Some(id) => return true,
                _ => {}
            }
        }
        false
    };
    let mut bucket_of: HashMap<usize, usize> = HashMap::new();
    let mut insert_before: HashMap<usize, Vec<usize>> = HashMap::new();
    for index in 0..items.len() {
        // Only anchor on the first output of a run.
        if kinds[index] != Batch::Output || (index > 0 && kinds[index - 1] == Batch::Output) {
            continue;
        }
        let mut pending: HashSet<&str> = (index..items.len())
            .take_while(|&probe| kinds[probe] == Batch::Output)
            .filter_map(call_id)
            .collect();
        let mut start = index;
        let mut saw_call = false;
        let mut messages = Vec::new();
        while start > 0 {
            let probe = start - 1;
            match kinds[probe] {
                Batch::Call => {
                    saw_call = true;
                    if let Some(id) = call_id(probe) {
                        pending.remove(id);
                    }
                }
                Batch::Output => {
                    // Cross an earlier output only when it belongs to this same
                    // interrupted batch; otherwise it closes a completed round.
                    let Some(id) = call_id(probe) else {
                        break;
                    };
                    if !earlier_batch_call(probe, id)
                        || !pending.iter().any(|owned| earlier_batch_call(probe, owned))
                    {
                        break;
                    }
                    pending.insert(id);
                }
                Batch::AssistantMessage => messages.push(probe),
                Batch::Other => break,
            }
            start -= 1;
        }
        if !saw_call || messages.is_empty() {
            continue;
        }
        for &message in messages.iter().rev() {
            // A wider batch can re-collect a message an earlier anchor scheduled.
            if let Some(previous) = bucket_of.insert(message, start) {
                if let Some(list) = insert_before.get_mut(&previous) {
                    list.retain(|&scheduled| scheduled != message);
                }
            }
            insert_before.entry(start).or_default().push(message);
        }
    }
    if bucket_of.is_empty() {
        return items;
    }
    let mut slots: Vec<Option<Value>> = items.into_iter().map(Some).collect();
    let mut hoisted = Vec::with_capacity(slots.len());
    for index in 0..slots.len() {
        if let Some(moved) = insert_before.get(&index) {
            hoisted.extend(moved.iter().filter_map(|&message| slots[message].take()));
        }
        if !bucket_of.contains_key(&index) {
            hoisted.extend(slots[index].take());
        }
    }
    hoisted
}

fn truncate_output(text: &str) -> String {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if units + width > ORPHAN_OUTPUT_LIMIT {
            let mut output = String::with_capacity(index + 20);
            output.push_str(&text[..index]);
            // JavaScript slice can end between a surrogate pair; Rust strings
            // cannot. Keep the valid-scalar prefix (at most 16,000 UTF-16 units)
            // rather than inventing a replacement character at that boundary.
            output.push_str("\n...[truncated]");
            return output;
        }
        units += width;
    }
    text.to_owned()
}

fn wyhash(input: &[u8]) -> u64 {
    const SECRET: [u64; 4] = [
        0xa0761d6478bd642f,
        0xe7037ed1a0b428db,
        0x8ebc6af09c88c6e3,
        0x589965cc75374cc3,
    ];
    fn mix(a: u64, b: u64) -> u64 {
        let product = u128::from(a) * u128::from(b);
        product as u64 ^ (product >> 64) as u64
    }
    fn read4(bytes: &[u8]) -> u64 {
        u64::from(u32::from_le_bytes(
            bytes[..4].try_into().expect("four bytes"),
        ))
    }
    fn read8(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes"))
    }
    let mut seed = mix(SECRET[0], SECRET[1]);
    let (a, b) = if input.len() <= 16 {
        if input.len() >= 4 {
            let end = input.len() - 4;
            let quarter = (input.len() >> 3) << 2;
            (
                (read4(input) << 32) | read4(&input[quarter..]),
                (read4(&input[end..]) << 32) | read4(&input[end - quarter..]),
            )
        } else if !input.is_empty() {
            (
                (u64::from(input[0]) << 16)
                    | (u64::from(input[input.len() >> 1]) << 8)
                    | u64::from(input[input.len() - 1]),
                0,
            )
        } else {
            (0, 0)
        }
    } else {
        let mut offset = 0;
        if input.len() >= 48 {
            let mut state = [seed; 3];
            while offset + 48 < input.len() {
                for i in 0..3 {
                    state[i] = mix(
                        read8(&input[offset + 16 * i..]) ^ SECRET[i + 1],
                        read8(&input[offset + 16 * i + 8..]) ^ state[i],
                    );
                }
                offset += 48;
            }
            seed = state[0] ^ state[1] ^ state[2];
        }
        while offset + 16 < input.len() {
            seed = mix(
                read8(&input[offset..]) ^ SECRET[1],
                read8(&input[offset + 8..]) ^ seed,
            );
            offset += 16;
        }
        (
            read8(&input[input.len() - 16..]),
            read8(&input[input.len() - 8..]),
        )
    };
    let product = u128::from(a ^ SECRET[1]) * u128::from(b ^ seed);
    mix(
        product as u64 ^ SECRET[0] ^ input.len() as u64,
        (product >> 64) as u64 ^ SECRET[1],
    )
}
