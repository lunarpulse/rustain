//! OpenAI Responses schema normalization, adapted from oh-my-pi normalize.ts.
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

use serde_json::{Map, Value, json};

/// Normalize only schema-valued positions, never literals in enum/const/default.
/// Unlike JavaScript object graphs, a serde JSON tree cannot contain cycles.
pub fn sanitize(value: Value) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    if object.is_empty() {
        return Value::Bool(true);
    }
    let one_of = if object.get("oneOf").is_some_and(Value::is_array) {
        object.remove("oneOf")
    } else {
        None
    };
    if object
        .get("pattern")
        .and_then(Value::as_str)
        .is_some_and(has_lookaround)
    {
        object.remove("pattern");
    }
    for (key, child) in &mut object {
        match key.as_str() {
            "properties" | "patternProperties" | "dependencies" | "dependentSchemas" | "$defs"
            | "definitions" => {
                if let Value::Object(map) = child {
                    let strip_patterns = key == "patternProperties";
                    let mut normalized = Map::new();
                    for (name, schema) in std::mem::take(map) {
                        let schema = sanitize(schema);
                        if strip_patterns && has_lookaround(&name) {
                            append_fallback(&mut normalized, schema);
                        } else {
                            normalized.insert(name, schema);
                        }
                    }
                    *map = normalized;
                }
            }
            "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                if let Value::Array(array) = child {
                    for schema in array {
                        *schema = sanitize(std::mem::take(schema));
                    }
                }
            }
            "items"
            | "additionalItems"
            | "contains"
            | "contentSchema"
            | "propertyNames"
            | "if"
            | "then"
            | "else"
            | "not"
            | "additionalProperties"
            | "unevaluatedItems"
            | "unevaluatedProperties" => {
                if child.is_object() {
                    *child = sanitize(std::mem::take(child));
                }
            }
            _ => {}
        }
    }
    if let Some(Value::Array(array)) = one_of {
        let rewritten = array.into_iter().map(sanitize);
        match object.get_mut("anyOf") {
            Some(Value::Array(existing)) => existing.extend(rewritten),
            _ => {
                object.insert("anyOf".into(), Value::Array(rewritten.collect()));
            }
        }
    }
    let object_type = object.get("type").is_some_and(|value| {
        value.as_str() == Some("object")
            || value
                .as_array()
                .is_some_and(|types| types.iter().any(|kind| kind.as_str() == Some("object")))
    });
    if object_type && !object.contains_key("properties") {
        object.insert("properties".into(), Value::Object(Map::new()));
    }
    if object.is_empty() {
        Value::Bool(true)
    } else {
        Value::Object(object)
    }
}

fn append_fallback(output: &mut Map<String, Value>, schema: Value) {
    match output.get_mut(".*") {
        None => {
            output.insert(".*".into(), schema);
        }
        Some(Value::Object(existing))
            if existing.len() == 1 && existing.get("anyOf").is_some_and(Value::is_array) =>
        {
            existing
                .get_mut("anyOf")
                .and_then(Value::as_array_mut)
                .expect("checked array")
                .push(schema);
        }
        Some(existing) => {
            *existing = json!({"anyOf":[std::mem::take(existing), schema]});
        }
    }
}

fn has_lookaround(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    for (start, _) in pattern.match_indices("(?") {
        let escapes = bytes[..start]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count();
        if escapes % 2 != 0 {
            continue;
        }
        let operator = &bytes[start + 2..];
        if operator.starts_with(b"=")
            || operator.starts_with(b"!")
            || operator.starts_with(b"<=")
            || operator.starts_with(b"<!")
        {
            return true;
        }
    }
    false
}
