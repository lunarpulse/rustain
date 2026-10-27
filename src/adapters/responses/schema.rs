//! OpenAI Responses schema normalization, adapted from oh-my-pi normalize.ts,
//! with the xAI root-union flatten (wire.ts) and tool quarantine
//! (strict-tool-validation.ts).
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

/// omp `flattenExclusiveRequiredRootUnion`: an object root whose `anyOf`/`oneOf`
/// holds only typeless required-key fragments loses that union (xAI rejects it).
pub fn flatten_exclusive_required_root_union(schema: Value) -> Value {
    let Value::Object(mut object) = schema else {
        return schema;
    };
    let key = if object.get("anyOf").is_some_and(Value::is_array) {
        "anyOf"
    } else if object.get("oneOf").is_some_and(Value::is_array) {
        "oneOf"
    } else {
        return Value::Object(object);
    };
    let typed_object = match object.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
        _ => false,
    };
    let flatten = (typed_object || object.get("properties").is_some_and(Value::is_object))
        && object[key]
            .as_array()
            .is_some_and(|union| !union.is_empty() && union.iter().all(exclusive_required_branch));
    if flatten {
        object.remove(key);
    }
    Value::Object(object)
}

fn exclusive_required_branch(branch: &Value) -> bool {
    let Some(branch) = branch.as_object() else {
        return false;
    };
    !branch.contains_key("type")
        && branch
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|names| {
                !names.is_empty()
                    && names
                        .iter()
                        .all(|name| name.as_str().is_some_and(|name| !name.is_empty()))
            })
        && branch
            .keys()
            .all(|key| matches!(key.as_str(), "required" | "description" | "title"))
}

/// omp `findStrictToolSchemaViolation`: the JSON-pointer-ish path of the first
/// `enum`/`const` that cannot satisfy its node's declared `type`, or, when
/// `reject_root_object_union`, of a root object union with a non-object branch.
pub fn strict_violation(schema: &Value, reject_root_object_union: bool) -> Option<String> {
    let mut path = String::from("#");
    violation(schema, &mut path, reject_root_object_union).then_some(path)
}

const SCHEMA_TYPES: [&str; 7] = [
    "string", "number", "integer", "boolean", "object", "array", "null",
];
const CHILD_MAP_KEYS: [&str; 5] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];
const CHILD_SCHEMA_KEYS: [&str; 10] = [
    "items",
    "contains",
    "not",
    "if",
    "then",
    "else",
    "propertyNames",
    "additionalProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
];
const CHILD_ARRAY_KEYS: [&str; 4] = ["anyOf", "oneOf", "allOf", "prefixItems"];

fn declared_types(node: &Map<String, Value>) -> Vec<&str> {
    match node.get("type") {
        Some(Value::String(kind)) if SCHEMA_TYPES.contains(&kind.as_str()) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .filter(|kind| SCHEMA_TYPES.contains(kind))
            .collect(),
        _ => Vec::new(),
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => true,
    }
}

/// Appends the offending segments to `path` and returns `true` on the first hit;
/// otherwise `path` is restored.
fn violation(schema: &Value, path: &mut String, reject_root_object_union: bool) -> bool {
    let mark = path.len();
    let node = match schema {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                path.push('/');
                path.push_str(&index.to_string());
                if violation(item, path, reject_root_object_union) {
                    return true;
                }
                path.truncate(mark);
            }
            return false;
        }
        Value::Object(node) => node,
        _ => return false,
    };
    let types = declared_types(node);
    if !types.is_empty() {
        let fits = |value: &Value| types.iter().any(|kind| matches_type(value, kind));
        if node
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| !values.iter().all(fits))
        {
            path.push_str("/enum");
            return true;
        }
        if node.get("const").is_some_and(|value| !fits(value)) {
            path.push_str("/const");
            return true;
        }
    }
    // JavaScript `typeof properties === "object"` also holds for arrays and null.
    if reject_root_object_union
        && mark == 1
        && (types.contains(&"object")
            || matches!(
                node.get("properties"),
                Some(Value::Object(_) | Value::Array(_) | Value::Null)
            ))
    {
        for key in ["anyOf", "oneOf"] {
            let Some(branches) = node.get(key).and_then(Value::as_array) else {
                continue;
            };
            if branches.iter().any(|branch| {
                branch
                    .as_object()
                    .is_none_or(|branch| !declared_types(branch).contains(&"object"))
            }) {
                path.push('/');
                path.push_str(key);
                return true;
            }
        }
    }
    for key in CHILD_MAP_KEYS {
        if let Some(children) = node.get(key).and_then(Value::as_object) {
            for (name, child) in children {
                path.push('/');
                path.push_str(key);
                path.push('/');
                path.push_str(name);
                if violation(child, path, reject_root_object_union) {
                    return true;
                }
                path.truncate(mark);
            }
        }
    }
    for key in CHILD_SCHEMA_KEYS {
        if let Some(child) = node.get(key) {
            path.push('/');
            path.push_str(key);
            if violation(child, path, reject_root_object_union) {
                return true;
            }
            path.truncate(mark);
        }
    }
    for key in CHILD_ARRAY_KEYS {
        if let Some(child @ Value::Array(_)) = node.get(key) {
            path.push('/');
            path.push_str(key);
            if violation(child, path, reject_root_object_union) {
                return true;
            }
            path.truncate(mark);
        }
    }
    false
}
