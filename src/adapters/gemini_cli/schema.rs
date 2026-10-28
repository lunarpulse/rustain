//! Gemini tool-schema normalization, adapted from oh-my-pi (ai/src/utils/schema/normalize.ts, draft.ts, dereference.ts, fields.ts, spill.ts, equality.ts; omptype/src/local-ref.ts).
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

// Port scope: `normalizeSchema` (normalize.ts:1269-1293) specialised to the
// `normalizeSchemaForGoogle` option set (normalize.ts:1295-1316):
//   coerceBooleanSubschemas "standard", unsupportedFields isGoogleUnsupportedSchemaField,
//   normalizeFieldNames, collapseNullFields, scalarizeTypeArrays, autoPropertyOrdering,
//   ensureObjectProperties, liftStrippedToDescription {format: "spill"},
//   inferTypeForBareEnum, stringEnumsOnly = true;
//   stripNullableKeyword, merge/collapse combiners, stripResidualCombinersFixpoint,
//   extractNullableFromUnions, dropNonScalarEnum, foldOneOfIntoAnyOf = false;
//   no rejectResidualIncompatibilities, no validateAndFallback.
// Branches only reachable under other option values (CCA/MCP/Moonshot/Cursor,
// "permissive" booleans, "paren" spill, custom lift keys) are not ported.
//
// Representation limits versus the JavaScript original:
// - serde_json here is built without `preserve_order`, so every object is
//   iterated in sorted key order where omp sees insertion order. This affects
//   auto `propertyOrdering` (sorted property names), the order of keywords
//   spilled into `description`, and key order inside spilled object values.
// - A serde tree cannot contain cycles or shared subtrees, so omp's
//   enter/exit/once/WeakMap cycle guards are dropped; `$ref` cycles are still
//   broken in `dereference_json_schema` exactly as omp does.
// - JavaScript prototype-chain lookups (`key in Record`) are treated as own-key
//   lookups.

use std::borrow::Cow;
use std::collections::HashSet;

use serde_json::{Map, Number, Value, json};

type JsonObject = Map<String, Value>;

/// omp `normalizeSchemaForGoogle`: draft-07 upgrade, local `$ref` inlining,
/// then the Google walker. Pure; never panics on any JSON input.
pub(crate) fn normalize_for_google(schema: &Value) -> Value {
    let upgraded = upgrade_json_schema_to_202012(schema);
    let dereferenced = dereference_json_schema(&upgraded);
    normalize_schema_node(
        &dereferenced,
        WalkOptions {
            inside_schema_map: false,
            boolean_is_subschema: true,
        },
    )
}

// ---------------------------------------------------------------------------
// fields.ts
// ---------------------------------------------------------------------------

/// fields.ts:16-67 `UNSUPPORTED_SCHEMA_FIELDS`.
const UNSUPPORTED_SCHEMA_FIELDS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$ref",
    "$defs",
    "$dynamicRef",
    "$dynamicAnchor",
    "examples",
    "prefixItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "patternProperties",
    "additionalProperties",
    "propertyNames",
    "minItems",
    "maxItems",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    "additionalItems",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "pattern",
    "format",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    "dependencies",
    "dependentSchemas",
    "dependentRequired",
    "x-mcp-header",
    "deprecated",
    "readOnly",
    "writeOnly",
    "$comment",
];

/// fields.ts:74-91 `LIFTABLE_TO_DESCRIPTION_FIELDS`.
const LIFTABLE_TO_DESCRIPTION_FIELDS: &[&str] = &[
    "pattern",
    "format",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "default",
    "examples",
];

/// Type names keyed in fields.ts:146-182 `CLOUD_CODE_ASSIST_TYPE_SPECIFIC_KEYS`.
const CCA_TYPE_NAMES: &[&str] = &[
    "array", "object", "string", "number", "integer", "boolean", "null",
];

/// fields.ts:146-182 `CLOUD_CODE_ASSIST_TYPE_SPECIFIC_KEYS[type] ?? {}`.
fn cloud_code_assist_type_specific_keys(type_name: &str) -> &'static [&'static str] {
    match type_name {
        "array" => &[
            "items",
            "prefixItems",
            "contains",
            "minContains",
            "maxContains",
            "minItems",
            "maxItems",
            "uniqueItems",
            "unevaluatedItems",
        ],
        "object" => &[
            "properties",
            "required",
            "additionalProperties",
            "patternProperties",
            "propertyNames",
            "minProperties",
            "maxProperties",
            "dependentRequired",
            "dependentSchemas",
            "unevaluatedProperties",
        ],
        "string" => &[
            "minLength",
            "maxLength",
            "pattern",
            "format",
            "contentEncoding",
            "contentMediaType",
        ],
        "number" | "integer" => &[
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ],
        _ => &[],
    }
}

/// fields.ts:188-198 `ALL_CCA_TYPE_SPECIFIC_KEYS` membership.
fn is_all_cca_type_specific_key(key: &str) -> bool {
    CCA_TYPE_NAMES
        .iter()
        .any(|type_name| cloud_code_assist_type_specific_keys(type_name).contains(&key))
}

/// normalize.ts:203-205.
fn is_google_unsupported_schema_field(key: &str) -> bool {
    UNSUPPORTED_SCHEMA_FIELDS.contains(&key)
}

/// normalize.ts:218-220.
fn is_default_liftable_to_description_field(key: &str) -> bool {
    LIFTABLE_TO_DESCRIPTION_FIELDS.contains(&key)
}

// ---------------------------------------------------------------------------
// equality.ts / spill.ts
// ---------------------------------------------------------------------------

/// equality.ts:4-30 `areJsonValuesEqual`; numbers compare as JS doubles
/// under `Object.is` (so `1` equals `1.0`, `0` differs from `-0.0`).
fn are_json_values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.to_bits() == y.to_bits(),
            _ => a == b,
        },
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| are_json_values_equal(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key)
                        .is_some_and(|other| are_json_values_equal(value, other))
                })
        }
        _ => left == right,
    }
}

/// spill.ts:18-43 `spillToDescription` with format "spill".
fn spill_to_description(node: &mut JsonObject, entries: &[(String, Value)]) {
    if entries.is_empty() {
        return;
    }
    let mut formatted = String::from("{");
    for (index, (key, value)) in entries.iter().enumerate() {
        if index > 0 {
            formatted.push_str(", ");
        }
        formatted.push_str(key);
        formatted.push_str(": ");
        write_js_json(value, &mut formatted);
    }
    formatted.push('}');
    let description = match node.get("description").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => format!("{existing}\n\n{formatted}"),
        _ => formatted,
    };
    node.insert("description".into(), Value::String(description));
}

/// `JSON.stringify` (spill.ts:5-7): same as serde_json except number rendering.
fn write_js_json(value: &Value, out: &mut String) {
    match value {
        Value::Number(number) => out.push_str(&js_number_to_string(number)),
        Value::Array(entries) => {
            out.push('[');
            for (index, entry) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_js_json(entry, out);
            }
            out.push(']');
        }
        Value::Object(object) => {
            out.push('{');
            for (index, (key, entry)) in object.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_js_json(entry, out);
            }
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// JavaScript `Number#toString` for finite doubles; integers print verbatim.
fn js_number_to_string(number: &Number) -> String {
    if number.is_i64() || number.is_u64() {
        return number.to_string();
    }
    let Some(float) = number.as_f64() else {
        return number.to_string();
    };
    if float == 0.0 {
        return "0".into();
    }
    if !float.is_finite() {
        return "null".into();
    }
    let magnitude = float.abs();
    if (1e-6..1e21).contains(&magnitude) {
        return format!("{float}");
    }
    let exponential = format!("{float:e}");
    match exponential.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            format!("{mantissa}e+{exponent}")
        }
        _ => exponential,
    }
}

// ---------------------------------------------------------------------------
// omptype local-ref.ts
// ---------------------------------------------------------------------------

/// local-ref.ts:14-27 `parseLocalRefPointer`.
fn parse_local_ref_pointer(reference: &str) -> Option<Vec<String>> {
    let fragment = reference.strip_prefix('#')?;
    let pointer = decode_uri_component(fragment)?;
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return None;
    }
    Some(
        pointer
            .split('/')
            .skip(1)
            .map(|token| token.replace("~1", "/").replace("~0", "~"))
            .collect(),
    )
}

/// local-ref.ts:33-41 `formatLocalRefPointer`.
fn format_local_ref_pointer(tokens: &[String]) -> String {
    if tokens.is_empty() {
        return "#".into();
    }
    let encoded: Vec<String> = tokens
        .iter()
        .map(|token| {
            encode_uri_component(&token.replace('~', "~0").replace('/', "~1")).replace("%24", "$")
        })
        .collect();
    format!("#/{}", encoded.join("/"))
}

/// `decodeURIComponent`; `None` where JavaScript throws `URIError`.
fn decode_uri_component(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes
                .get(index + 1)
                .and_then(|b| (*b as char).to_digit(16))?;
            let low = bytes
                .get(index + 2)
                .and_then(|b| (*b as char).to_digit(16))?;
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// `encodeURIComponent`.
fn encode_uri_component(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// draft.ts
// ---------------------------------------------------------------------------

/// draft.ts:6.
const JSON_SCHEMA_DRAFT_2020_12_URI: &str = "https://json-schema.org/draft/2020-12/schema";

/// draft.ts:9-14.
const DRAFT_07_SCHEMA_URIS: &[&str] = &[
    "http://json-schema.org/draft-07/schema#",
    "https://json-schema.org/draft-07/schema#",
    "http://json-schema.org/draft-07/schema",
    "https://json-schema.org/draft-07/schema",
];

/// draft.ts:21 `SCHEMA_MAP_KEYS`.
const DRAFT_SCHEMA_MAP_KEYS: &[&str] = &["properties", "patternProperties", "dependentSchemas"];

/// draft.ts:27-36.
const NON_SCHEMA_VALUE_KEYS: &[&str] = &[
    "const",
    "default",
    "enum",
    "example",
    "examples",
    "required",
    "dependentRequired",
    "type",
];

fn is_draft_07_schema_uri(uri: &str) -> bool {
    DRAFT_07_SCHEMA_URIS.contains(&uri)
}

/// draft.ts:346-349 `upgradeJsonSchemaTo202012`: input unchanged unless the
/// pre-check finds a draft-07 shape.
fn upgrade_json_schema_to_202012(schema: &Value) -> Cow<'_, Value> {
    if !schema_needs_draft_202012_upgrade(schema) {
        return Cow::Borrowed(schema);
    }
    Cow::Owned(upgrade_json_schema_to_202012_impl(schema))
}

/// draft.ts:190-196.
fn schema_map_needs_draft_202012_upgrade(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.values().any(schema_needs_draft_202012_upgrade))
}

/// draft.ts:205-236 `schemaNeedsDraft202012UpgradeImpl`.
fn schema_needs_draft_202012_upgrade(value: &Value) -> bool {
    match value {
        Value::Array(entries) => entries.iter().any(schema_needs_draft_202012_upgrade),
        Value::Object(object) => {
            for (key, entry) in object {
                match key.as_str() {
                    "$schema" => {
                        if entry.as_str().is_some_and(is_draft_07_schema_uri) {
                            return true;
                        }
                    }
                    "definitions" | "dependencies" | "additionalItems" | "nullable" => return true,
                    "$ref" => {
                        if entry
                            .as_str()
                            .is_some_and(|reference| reference.starts_with("#/definitions/"))
                        {
                            return true;
                        }
                    }
                    "items" if entry.is_array() => return true,
                    other if other == "$defs" || DRAFT_SCHEMA_MAP_KEYS.contains(&other) => {
                        if schema_map_needs_draft_202012_upgrade(entry) {
                            return true;
                        }
                    }
                    other if NON_SCHEMA_VALUE_KEYS.contains(&other) => {}
                    _ => {
                        if schema_needs_draft_202012_upgrade(entry) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// draft.ts:244-329 `upgradeJsonSchemaTo202012Impl`.
fn upgrade_json_schema_to_202012_impl(value: &Value) -> Value {
    match value {
        Value::Array(entries) => Value::Array(
            entries
                .iter()
                .map(upgrade_json_schema_to_202012_impl)
                .collect(),
        ),
        Value::Object(object) => upgrade_json_schema_object(object),
        _ => value.clone(),
    }
}

fn upgrade_json_schema_object(value: &JsonObject) -> Value {
    let mut result = JsonObject::new();
    for (key, entry) in value {
        match key.as_str() {
            "definitions" | "$defs" => {
                if let Value::Object(map) = entry {
                    merge_schema_map(&mut result, "$defs", map);
                }
            }
            other if DRAFT_SCHEMA_MAP_KEYS.contains(&other) => {
                copy_schema_map(&mut result, key, entry);
            }
            other if NON_SCHEMA_VALUE_KEYS.contains(&other) => {
                result.insert(key.clone(), entry.clone());
            }
            "dependencies" | "additionalItems" | "nullable" => {}
            "$schema" => {
                let upgraded = match entry.as_str() {
                    Some(uri) if is_draft_07_schema_uri(uri) => {
                        Value::String(JSON_SCHEMA_DRAFT_2020_12_URI.into())
                    }
                    _ => entry.clone(),
                };
                result.insert(key.clone(), upgraded);
            }
            "$ref" if entry.is_string() => {
                let reference = entry.as_str().unwrap_or_default();
                result.insert(key.clone(), Value::String(convert_ref(reference)));
            }
            "items" if entry.is_array() => {}
            _ => {
                result.insert(key.clone(), upgrade_json_schema_to_202012_impl(entry));
            }
        }
    }

    // Draft-07 tuple form (draft.ts:303-314): `items: [..]` → `prefixItems`,
    // `additionalItems` → `items`.
    if let Some(Value::Array(items)) = value.get("items") {
        let converted: Vec<Value> = items
            .iter()
            .map(upgrade_json_schema_to_202012_impl)
            .collect();
        let merged = merge_prefix_items(result.remove("prefixItems"), converted);
        result.insert("prefixItems".into(), Value::Array(merged));
        match value.get("additionalItems") {
            Some(additional) if *additional != Value::Bool(true) => {
                result.insert(
                    "items".into(),
                    upgrade_json_schema_to_202012_impl(additional),
                );
            }
            _ => {
                result.remove("items");
            }
        }
    }

    convert_dependencies(value, &mut result);

    if value.get("nullable") == Some(&Value::Bool(true)) {
        return make_nullable(result);
    }
    Value::Object(result)
}

/// draft.ts:44-48 `convertRef`: `#/definitions/Foo` → `#/$defs/Foo`.
fn convert_ref(value: &str) -> String {
    match parse_local_ref_pointer(value) {
        Some(pointer) if pointer.first().is_some_and(|head| head == "definitions") => {
            let mut tokens = Vec::with_capacity(pointer.len());
            tokens.push("$defs".to_owned());
            tokens.extend(pointer.into_iter().skip(1));
            format_local_ref_pointer(&tokens)
        }
        _ => value.to_owned(),
    }
}

/// draft.ts:51-57 `getObjectMap`.
fn get_object_map<'a>(target: &'a mut JsonObject, key: &str) -> &'a mut JsonObject {
    let slot = target
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(JsonObject::new()));
    if !slot.is_object() {
        *slot = Value::Object(JsonObject::new());
    }
    match slot {
        Value::Object(map) => map,
        _ => unreachable!("slot was just set to an object"),
    }
}

/// draft.ts:60-65 `mergeSchemaMap`.
fn merge_schema_map(target: &mut JsonObject, key: &str, value: &JsonObject) {
    let map = get_object_map(target, key);
    for (name, entry) in value {
        map.insert(name.clone(), upgrade_json_schema_to_202012_impl(entry));
    }
}

/// draft.ts:67-73 `copySchemaMap`.
fn copy_schema_map(target: &mut JsonObject, key: &str, value: &Value) {
    match value {
        Value::Object(map) => merge_schema_map(target, key, map),
        _ => {
            target.insert(key.to_owned(), value.clone());
        }
    }
}

/// draft.ts:82-88 `combineSchemas`; `None` is JavaScript `undefined`.
fn combine_schemas(left: Option<Value>, right: Value) -> Value {
    let left = match left {
        None | Some(Value::Bool(true)) => return right,
        Some(left) => left,
    };
    if right == Value::Bool(true) {
        return left;
    }
    if left == Value::Bool(false) || right == Value::Bool(false) {
        return Value::Bool(false);
    }
    if are_json_values_equal(&left, &right) {
        return left;
    }
    let mut all_of = JsonObject::new();
    all_of.insert("allOf".into(), Value::Array(vec![left, right]));
    Value::Object(all_of)
}

/// draft.ts:91-99 `mergeArrayValues`.
fn merge_array_values(merged: &mut Vec<Value>, right: Vec<Value>) {
    for value in right {
        if !merged
            .iter()
            .any(|existing| are_json_values_equal(existing, &value))
        {
            merged.push(value);
        }
    }
}

/// draft.ts:106-113 `mergePrefixItems`.
fn merge_prefix_items(existing: Option<Value>, converted_items: Vec<Value>) -> Vec<Value> {
    let Some(Value::Array(mut merged)) = existing else {
        return converted_items;
    };
    for (index, item) in converted_items.into_iter().enumerate() {
        if let Some(slot) = merged.get_mut(index) {
            let previous = std::mem::take(slot);
            *slot = combine_schemas(Some(previous), item);
        } else {
            merged.push(item);
        }
    }
    merged
}

/// draft.ts:116-126 `mergeDependentRequired`.
fn merge_dependent_required(target: &mut JsonObject, key: &str, deps: Vec<Value>) {
    let dependent_required = get_object_map(target, "dependentRequired");
    match dependent_required.get_mut(key) {
        None => {
            dependent_required.insert(key.to_owned(), Value::Array(deps));
        }
        Some(Value::Array(existing)) => merge_array_values(existing, deps),
        Some(_) => {}
    }
}

/// draft.ts:129-132 `mergeDependentSchema`.
fn merge_dependent_schema(target: &mut JsonObject, key: &str, schema: Value) {
    let dependent_schemas = get_object_map(target, "dependentSchemas");
    let existing = dependent_schemas.remove(key);
    dependent_schemas.insert(key.to_owned(), combine_schemas(existing, schema));
}

/// draft.ts:140-152 `convertDependencies`.
fn convert_dependencies(source: &JsonObject, target: &mut JsonObject) {
    let Some(Value::Object(dependencies)) = source.get("dependencies") else {
        return;
    };
    for (key, dependency) in dependencies {
        match upgrade_json_schema_to_202012_impl(dependency) {
            Value::Array(deps) => merge_dependent_required(target, key, deps),
            schema => merge_dependent_schema(target, key, schema),
        }
    }
}

/// draft.ts:155-157 `hasNullType`.
fn has_null_type(type_value: Option<&Value>) -> bool {
    match type_value {
        Some(Value::String(type_name)) => type_name == "null",
        Some(Value::Array(types)) => types.iter().any(|entry| entry.as_str() == Some("null")),
        _ => false,
    }
}

/// draft.ts:160-162 `hasNullVariant`.
fn has_null_variant(variants: &[Value]) -> bool {
    variants
        .iter()
        .any(|variant| variant.is_object() && has_null_type(variant.get("type")))
}

/// draft.ts:172-187 `makeNullable`.
fn make_nullable(mut schema: JsonObject) -> Value {
    if let Some(slot) = schema.get_mut("type") {
        if let Value::String(type_name) = slot {
            if type_name.as_str() != "null" {
                let type_name = slot.take();
                *slot = Value::Array(vec![type_name, Value::String("null".into())]);
            }
            return Value::Object(schema);
        }
        if let Value::Array(types) = slot {
            if !types.iter().any(|entry| entry.as_str() == Some("null")) {
                types.push(Value::String("null".into()));
            }
            return Value::Object(schema);
        }
    }
    if let Some(Value::Array(any_of)) = schema.get_mut("anyOf") {
        if !has_null_variant(any_of) {
            any_of.push(json!({ "type": "null" }));
        }
        return Value::Object(schema);
    }
    let mut wrapper = JsonObject::new();
    wrapper.insert(
        "anyOf".into(),
        Value::Array(vec![Value::Object(schema), json!({ "type": "null" })]),
    );
    Value::Object(wrapper)
}

// ---------------------------------------------------------------------------
// dereference.ts
// ---------------------------------------------------------------------------

/// dereference.ts:16-21.
const DEREF_SCHEMA_MAP_KEYS: &[&str] = &[
    "properties",
    "patternProperties",
    "dependencies",
    "dependentSchemas",
];
/// dereference.ts:22.
const DEREF_SCHEMA_ARRAY_KEYS: &[&str] = &["anyOf", "oneOf", "allOf", "prefixItems"];
/// dereference.ts:23-37.
const DEREF_SCHEMA_VALUE_KEYS: &[&str] = &[
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
];

/// dereference.ts:123-131 `dereferenceJsonSchema`: inline local `$ref`s from the
/// root `$defs`/`definitions`, which are dropped from the output.
fn dereference_json_schema(schema: &Value) -> Cow<'_, Value> {
    let Value::Object(root) = schema else {
        return Cow::Borrowed(schema);
    };
    if !root.contains_key("$defs") && !root.contains_key("definitions") {
        return Cow::Borrowed(schema);
    }
    Cow::Owned(dereference_node(schema, root, &mut HashSet::new()))
}

/// dereference.ts:39-50 `dereferenceKeyword`.
fn dereference_keyword(
    key: &str,
    value: &Value,
    root: &JsonObject,
    visiting: &mut HashSet<String>,
) -> Value {
    match value {
        Value::Object(map) if DEREF_SCHEMA_MAP_KEYS.contains(&key) => Value::Object(
            map.iter()
                .map(|(name, entry)| (name.clone(), dereference_node(entry, root, visiting)))
                .collect(),
        ),
        Value::Array(items) if DEREF_SCHEMA_ARRAY_KEYS.contains(&key) || key == "items" => {
            Value::Array(
                items
                    .iter()
                    .map(|item| dereference_node(item, root, visiting))
                    .collect(),
            )
        }
        _ if DEREF_SCHEMA_VALUE_KEYS.contains(&key) => dereference_node(value, root, visiting),
        _ => value.clone(),
    }
}

/// dereference.ts:56-66 `resolveLocalRef`: only `#/$defs/<name>` and
/// `#/definitions/<name>` resolving to an object.
fn resolve_local_ref<'a>(reference: &str, root: &'a JsonObject) -> Option<&'a Value> {
    let pointer = parse_local_ref_pointer(reference)?;
    let [defs_key, name] = pointer.as_slice() else {
        return None;
    };
    if defs_key != "$defs" && defs_key != "definitions" {
        return None;
    }
    root.get(defs_key)?
        .as_object()?
        .get(name)
        .filter(|resolved| resolved.is_object())
}

fn is_definition_key(key: &str) -> bool {
    matches!(key, "$ref" | "$defs" | "definitions")
}

/// dereference.ts:71-111 `dereferenceNode`. A ref already on the resolution
/// path becomes `{}`; unresolvable refs are returned untouched.
fn dereference_node(node: &Value, root: &JsonObject, visiting: &mut HashSet<String>) -> Value {
    let Value::Object(object) = node else {
        return node.clone();
    };
    if let Some(Value::String(reference)) = object.get("$ref") {
        if visiting.contains(reference) {
            return Value::Object(JsonObject::new());
        }
        let Some(resolved) = resolve_local_ref(reference, root) else {
            return node.clone();
        };
        visiting.insert(reference.clone());
        let inlined = dereference_node(resolved, root, visiting);
        visiting.remove(reference);

        let has_siblings = object.keys().any(|key| !is_definition_key(key));
        if !has_siblings {
            return inlined;
        }
        let Value::Object(mut merged) = inlined else {
            return inlined;
        };
        for (key, entry) in object {
            if is_definition_key(key) {
                continue;
            }
            merged.insert(key.clone(), dereference_keyword(key, entry, root, visiting));
        }
        return Value::Object(merged);
    }

    let mut result = JsonObject::new();
    for (key, entry) in object {
        if key == "$defs" || key == "definitions" {
            continue;
        }
        result.insert(key.clone(), dereference_keyword(key, entry, root, visiting));
    }
    Value::Object(result)
}

// ---------------------------------------------------------------------------
// normalize.ts walker
// ---------------------------------------------------------------------------

/// normalize.ts:93.
const JSON_SCHEMA_COMBINERS: [&str; 2] = ["anyOf", "oneOf"];

/// normalize.ts:102-114.
const SUBSCHEMA_VALUE_KEYS: &[&str] = &[
    "items",
    "additionalItems",
    "unevaluatedItems",
    "not",
    "if",
    "then",
    "else",
    "contains",
    "propertyNames",
    "contentSchema",
];
/// normalize.ts:120-123.
const BOOLEAN_OR_SCHEMA_VALUE_KEYS: &[&str] = &["additionalProperties", "unevaluatedProperties"];
/// normalize.ts:126-131.
const SUBSCHEMA_ARRAY_KEYS: &[&str] = &["anyOf", "oneOf", "allOf", "prefixItems"];
/// normalize.ts:134-141.
const SUBSCHEMA_MAP_KEYS: &[&str] = &[
    "properties",
    "patternProperties",
    "dependencies",
    "dependentSchemas",
    "$defs",
    "definitions",
];

/// normalize.ts:86-91 `SNAKE_TO_CAMEL_RENAMES`.
fn snake_to_camel_rename(key: &str) -> Option<&'static str> {
    match key {
        "additional_properties" => Some("additionalProperties"),
        "any_of" => Some("anyOf"),
        "prefix_items" => Some("prefixItems"),
        "property_ordering" => Some("propertyOrdering"),
        _ => None,
    }
}

/// normalize.ts:67-76 walk-only options; every other option is a Google constant.
#[derive(Clone, Copy)]
struct WalkOptions {
    inside_schema_map: bool,
    boolean_is_subschema: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SchemaChildKind {
    Schema,
    Map,
}

/// normalize.ts:146-155 `classifySchemaChild`.
fn classify_schema_child(
    key: &str,
    value: &Value,
    inside_schema_map: bool,
) -> Option<SchemaChildKind> {
    if inside_schema_map {
        return Some(SchemaChildKind::Schema);
    }
    let normalized_key = snake_to_camel_rename(key).unwrap_or(key);
    if SUBSCHEMA_MAP_KEYS.contains(&normalized_key) {
        return Some(SchemaChildKind::Map);
    }
    if SUBSCHEMA_VALUE_KEYS.contains(&normalized_key)
        || SUBSCHEMA_ARRAY_KEYS.contains(&normalized_key)
    {
        return Some(SchemaChildKind::Schema);
    }
    if BOOLEAN_OR_SCHEMA_VALUE_KEYS.contains(&normalized_key) && value.is_object() {
        return Some(SchemaChildKind::Schema);
    }
    None
}

/// normalize.ts:157-196 `hasUnrepresentableGoogleEnumConstraint`.
fn has_unrepresentable_google_enum_constraint(value: &Value, inside_schema_map: bool) -> bool {
    match value {
        Value::Array(entries) => entries
            .iter()
            .any(|entry| has_unrepresentable_google_enum_constraint(entry, false)),
        Value::Object(object) => {
            if inside_schema_map {
                return object
                    .values()
                    .any(|entry| has_unrepresentable_google_enum_constraint(entry, false));
            }
            if let Some(Value::Array(values)) = object.get("enum")
                && (values.is_empty() || values.iter().any(|entry| !entry.is_string()))
            {
                return true;
            }
            if object
                .get("const")
                .is_some_and(|constant| !constant.is_string())
            {
                return true;
            }
            object.iter().any(|(key, entry)| {
                classify_schema_child(key, entry, false).is_some_and(|kind| {
                    has_unrepresentable_google_enum_constraint(entry, kind == SchemaChildKind::Map)
                })
            })
        }
        _ => false,
    }
}

/// normalize.ts:228-249 `applySnakeCaseRenames`; snake_case wins a collision.
/// `None` when nothing needs renaming.
fn apply_snake_case_renames(object: &JsonObject) -> Option<JsonObject> {
    if !object
        .keys()
        .any(|key| snake_to_camel_rename(key).is_some())
    {
        return None;
    }
    let mut out = JsonObject::new();
    for (key, value) in object {
        if let Some(renamed) = snake_to_camel_rename(key) {
            out.insert(renamed.to_owned(), value.clone());
        } else if !out.contains_key(key) {
            out.insert(key.clone(), value.clone());
        }
    }
    Some(out)
}

fn is_null_type_schema(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("null")
}

/// normalize.ts:257-296 `preHandleNullFields` (python-genai `handle_null_fields`).
/// `None` when unchanged.
fn pre_handle_null_fields(object: &JsonObject) -> Option<JsonObject> {
    if object.get("type").and_then(Value::as_str) == Some("null") {
        let mut out: JsonObject = object
            .iter()
            .filter(|(key, _)| key.as_str() != "type")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        out.insert("nullable".into(), Value::Bool(true));
        return Some(out);
    }
    let Some(Value::Array(variants)) = object.get("anyOf") else {
        return None;
    };
    if !variants.iter().any(is_null_type_schema) {
        return None;
    }
    let kept: Vec<&Value> = variants
        .iter()
        .filter(|variant| !is_null_type_schema(variant))
        .collect();
    let mut out = object.clone();
    out.insert("nullable".into(), Value::Bool(true));
    match kept.as_slice() {
        [] => {
            out.remove("anyOf");
        }
        [Value::Object(only)] => {
            out.remove("anyOf");
            for (key, value) in only {
                if !out.contains_key(key) {
                    out.insert(key.clone(), value.clone());
                }
            }
        }
        _ => {
            out.insert(
                "anyOf".into(),
                Value::Array(kept.iter().map(|variant| (*variant).clone()).collect()),
            );
        }
    }
    Some(out)
}

/// normalize.ts:307-333 `splitTypeArrayIntoAnyOf`: a multi-type `type` array
/// becomes one `anyOf` branch per non-null type carrying only that type's
/// keywords; a `null` member becomes `nullable: true`. `None` when unchanged.
fn split_type_array_into_any_of(object: &JsonObject) -> Option<JsonObject> {
    let Some(Value::Array(type_list)) = object.get("type") else {
        return None;
    };
    if object.get("anyOf").is_some_and(Value::is_array) {
        return None;
    }
    let mut types: Vec<&str> = Vec::new();
    for entry in type_list {
        if let Some(type_name) = entry.as_str()
            && type_name != "null"
            && !types.contains(&type_name)
        {
            types.push(type_name);
        }
    }
    if types.len() < 2 {
        return None;
    }
    let mut branches: Vec<(JsonObject, &'static [&'static str])> = types
        .iter()
        .map(|type_name| {
            let mut schema = JsonObject::new();
            schema.insert("type".into(), Value::String((*type_name).to_owned()));
            (schema, cloud_code_assist_type_specific_keys(type_name))
        })
        .collect();
    let mut out = JsonObject::new();
    for (key, value) in object {
        if key == "type" {
            continue;
        }
        if !is_all_cca_type_specific_key(key) {
            out.insert(key.clone(), value.clone());
            continue;
        }
        for (schema, keys) in &mut branches {
            if keys.contains(&key.as_str()) {
                schema.insert(key.clone(), value.clone());
            }
        }
    }
    if type_list.iter().any(|entry| entry.as_str() == Some("null")) {
        out.insert("nullable".into(), Value::Bool(true));
    }
    out.insert(
        "anyOf".into(),
        Value::Array(
            branches
                .into_iter()
                .map(|(schema, _)| Value::Object(schema))
                .collect(),
        ),
    );
    Some(out)
}

/// normalize.ts:336-341 `dropForeignTypeKeywords`.
fn drop_foreign_type_keywords(schema: &mut JsonObject, type_name: &str) {
    let allowed = cloud_code_assist_type_specific_keys(type_name);
    schema.retain(|key, _| !is_all_cca_type_specific_key(key) || allowed.contains(&key.as_str()));
}

/// normalize.ts:347-362 `inferJsonSchemaTypeFromValue` (always defined for JSON).
fn infer_json_schema_type_from_value(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "object",
    }
}

/// normalize.ts:364-368 `pushEnumValue`.
fn push_enum_value(values: &mut Vec<Value>, value: &Value) {
    if !values
        .iter()
        .any(|existing| are_json_values_equal(existing, value))
    {
        values.push(value.clone());
    }
}

/// normalize.ts:370-383 `pushStrippedDescriptionEntry` (default liftable keys).
fn push_stripped_description_entry(spill: &mut Vec<(String, Value)>, key: &str, value: &Value) {
    if is_default_liftable_to_description_field(key) {
        spill.push((key.to_owned(), value.clone()));
    }
}

/// JavaScript truthiness of an optional property (`!result.type`).
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number
            .as_f64()
            .is_some_and(|float| float != 0.0 && !float.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

fn distinct_count(items: &[&str]) -> usize {
    let mut seen: Vec<&str> = Vec::with_capacity(items.len());
    for &item in items {
        if !seen.contains(&item) {
            seen.push(item);
        }
    }
    seen.len()
}

fn normalize_schema_child(key: &str, entry: &Value, options: WalkOptions) -> Value {
    match classify_schema_child(key, entry, options.inside_schema_map) {
        Some(kind) => normalize_schema_node(
            entry,
            WalkOptions {
                inside_schema_map: kind == SchemaChildKind::Map,
                boolean_is_subschema: kind == SchemaChildKind::Schema,
            },
        ),
        None => entry.clone(),
    }
}

/// normalize.ts:395-424 `normalizeSchemaNode`. Booleans in a subschema slot
/// coerce ("standard"): `true` → `{}`, `false` → `{not: {}}`.
fn normalize_schema_node(value: &Value, options: WalkOptions) -> Value {
    match value {
        Value::Array(entries) => Value::Array(
            entries
                .iter()
                .map(|entry| normalize_schema_node(entry, options))
                .collect(),
        ),
        Value::Bool(flag) if options.boolean_is_subschema => {
            if *flag {
                Value::Object(JsonObject::new())
            } else {
                json!({ "not": {} })
            }
        }
        Value::Object(object) => Value::Object(normalize_schema_object_node(object, options)),
        _ => value.clone(),
    }
}

/// normalize.ts:426-592 `normalizeSchemaObjectNode`.
fn normalize_schema_object_node(value: &JsonObject, options: WalkOptions) -> JsonObject {
    let mut object: Cow<'_, JsonObject> = Cow::Borrowed(value);
    if !options.inside_schema_map {
        if let Some(renamed) = apply_snake_case_renames(&object) {
            object = Cow::Owned(renamed);
        }
        if let Some(split) = split_type_array_into_any_of(&object) {
            object = Cow::Owned(split);
        }
        if let Some(collapsed) = pre_handle_null_fields(&object) {
            object = Cow::Owned(collapsed);
        }
    }
    let object: &JsonObject = &object;
    let mut result = JsonObject::new();
    let mut spill: Vec<(String, Value)> = Vec::new();

    // normalize.ts:436-502: an anyOf/oneOf whose every branch carries `const`
    // becomes an `enum` (the combiner itself is dropped).
    for combiner in JSON_SCHEMA_COMBINERS {
        let Some(Value::Array(variants)) = object.get(combiner) else {
            continue;
        };
        let all_have_const = variants
            .iter()
            .all(|variant| variant.as_object().is_some_and(|v| v.contains_key("const")));
        if !all_have_const || variants.is_empty() {
            continue;
        }

        let mut deduped_enum: Vec<Value> = Vec::new();
        for variant in variants {
            if let Some(constant) = variant.get("const") {
                push_enum_value(&mut deduped_enum, constant);
            }
        }

        let explicit_types: Vec<&str> = variants
            .iter()
            .filter_map(|variant| variant.get("type").and_then(Value::as_str))
            .collect();
        let all_have_same_explicit_type = explicit_types.len() == variants.len()
            && explicit_types
                .iter()
                .all(|type_name| *type_name == explicit_types[0]);
        let first_explicit_type = explicit_types.first().copied().filter(|t| !t.is_empty());
        match first_explicit_type {
            Some(type_name) if all_have_same_explicit_type => {
                result.insert("type".into(), Value::String(type_name.to_owned()));
            }
            _ => {
                let inferred_types: Vec<&str> = deduped_enum
                    .iter()
                    .map(infer_json_schema_type_from_value)
                    .collect();
                if distinct_count(&inferred_types) == 1 {
                    result.insert("type".into(), Value::String(inferred_types[0].to_owned()));
                } else {
                    let non_null_inferred: Vec<&str> = inferred_types
                        .iter()
                        .copied()
                        .filter(|type_name| *type_name != "null")
                        .collect();
                    if inferred_types.contains(&"null") && distinct_count(&non_null_inferred) == 1 {
                        result.insert(
                            "type".into(),
                            Value::String(non_null_inferred[0].to_owned()),
                        );
                        result.insert("nullable".into(), Value::Bool(true));
                    }
                }
            }
        }
        result.insert("enum".into(), Value::Array(deduped_enum));

        for (key, entry) in object {
            if key == combiner || result.contains_key(key) {
                continue;
            }
            if !options.inside_schema_map && is_google_unsupported_schema_field(key) {
                push_stripped_description_entry(&mut spill, key, entry);
                continue;
            }
            if !options.inside_schema_map
                && key == "not"
                && has_unrepresentable_google_enum_constraint(entry, false)
            {
                continue;
            }
            result.insert(key.clone(), normalize_schema_child(key, entry, options));
        }
        spill_to_description(&mut result, &spill);
        return apply_node_post_processing(result, options);
    }

    let mut const_value: Option<&Value> = None;
    for (key, entry) in object {
        if !options.inside_schema_map && is_google_unsupported_schema_field(key) {
            push_stripped_description_entry(&mut spill, key, entry);
            continue;
        }
        // omp captures `const` even inside a schema map (a property literally
        // named `const`); ported as-is.
        if key == "const" {
            const_value = Some(entry);
            continue;
        }
        // stringEnumsOnly: Google cannot express a negated non-string enum.
        if !options.inside_schema_map
            && key == "not"
            && has_unrepresentable_google_enum_constraint(entry, false)
        {
            continue;
        }
        result.insert(key.clone(), normalize_schema_child(key, entry, options));
    }

    // normalize.ts:535-543 scalarizeTypeArrays.
    if let Some(Value::Array(type_list)) = result.get("type") {
        let types: Vec<&str> = type_list.iter().filter_map(Value::as_str).collect();
        let includes_null = types.contains(&"null");
        let chosen = types
            .iter()
            .find(|type_name| **type_name != "null")
            .or(types.first())
            .map(|type_name| (*type_name).to_owned());
        if includes_null {
            result.insert("nullable".into(), Value::Bool(true));
        }
        match chosen {
            Some(type_name) => {
                drop_foreign_type_keywords(&mut result, &type_name);
                result.insert("type".into(), Value::String(type_name));
            }
            None => {
                result.remove("type");
            }
        }
    }

    // normalize.ts:544-551 const → singleton enum.
    if let Some(constant) = const_value {
        let mut existing_enum = match result.remove("enum") {
            Some(Value::Array(values)) => values,
            _ => Vec::new(),
        };
        push_enum_value(&mut existing_enum, constant);
        result.insert("enum".into(), Value::Array(existing_enum));
        if !is_truthy(result.get("type")) {
            result.insert(
                "type".into(),
                Value::String(infer_json_schema_type_from_value(constant).to_owned()),
            );
        }
    }

    // normalize.ts:553-565 inferTypeForBareEnum.
    if !is_truthy(result.get("type"))
        && !result.get("anyOf").is_some_and(Value::is_array)
        && !result.get("oneOf").is_some_and(Value::is_array)
        && let Some(Value::Array(values)) = result.get("enum")
        && let Some(first) = values.first()
    {
        let first_type = infer_json_schema_type_from_value(first);
        if values
            .iter()
            .all(|entry| infer_json_schema_type_from_value(entry) == first_type)
        {
            result.insert("type".into(), Value::String(first_type.to_owned()));
        }
    }

    // normalize.ts:567-570 collapseNullFields.
    if result.get("type").and_then(Value::as_str) == Some("null") {
        result.remove("type");
        result.insert("nullable".into(), Value::Bool(true));
    }

    let is_object_type = result.get("type").and_then(Value::as_str) == Some("object");

    // normalize.ts:572-584 autoPropertyOrdering; key order is serde's sorted order.
    if is_object_type
        && !result.contains_key("propertyOrdering")
        && let Some(Value::Object(properties)) = result.get("properties")
        && properties.len() > 1
    {
        let keys: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
        result.insert("propertyOrdering".into(), Value::Array(keys));
    }

    // normalize.ts:586-588 ensureObjectProperties.
    if is_object_type && !result.contains_key("properties") {
        result.insert("properties".into(), Value::Object(JsonObject::new()));
    }

    spill_to_description(&mut result, &spill);
    apply_node_post_processing(result, options)
}

/// normalize.ts:594-605 `applyNodePostProcessing`; with the Google options
/// only `dropNonStringEnumForGoogle` runs, and only in subschema slots.
fn apply_node_post_processing(mut schema: JsonObject, options: WalkOptions) -> JsonObject {
    if options.boolean_is_subschema {
        drop_non_string_enum_for_google(&mut schema);
    }
    schema
}

/// normalize.ts:625-629 `dropNonStringEnumForGoogle`.
fn drop_non_string_enum_for_google(schema: &mut JsonObject) {
    let Some(Value::Array(values)) = schema.get("enum") else {
        return;
    };
    let is_string_enum = !values.is_empty() && values.iter().all(Value::is_string);
    if !is_string_enum {
        schema.remove("enum");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_rows(rows: Vec<(&str, Value, Value)>) {
        for (name, input, expected) in rows {
            assert_eq!(normalize_for_google(&input), expected, "row: {name}");
        }
    }

    /// Cases copied from omp tests that call `normalizeSchemaForGoogle` (or
    /// `convertTools` for a Gemini model). Rows marked `[sorted]` differ from
    /// omp only in `propertyOrdering`/spill order, which follows serde's sorted
    /// keys instead of JavaScript insertion order.
    #[test]
    fn omp_fixture_rows() {
        let dict_schema = json!({
            "type": "object",
            "title": "Fruit Basket",
            "description": "A structured representation of a fruit basket",
            "required": ["fruit"],
            "properties": {
                "fruit": {
                    "type": "array",
                    "description": "An ordered list of the fruit in the basket",
                    "items": {
                        "description": "A piece of fruit",
                        "anyOf": [
                            {
                                "title": "Apple",
                                "description": "Describes an apple",
                                "type": "object",
                                "properties": {
                                    "type": {"type": "string", "description": "Always 'apple'"},
                                    "color": {"type": "string", "description": "The color of the apple"}
                                },
                                "propertyOrdering": ["type", "color"],
                                "required": ["type", "color"]
                            },
                            {
                                "title": "Orange",
                                "description": "Describes an orange",
                                "type": "object",
                                "properties": {
                                    "type": {"type": "string", "description": "Always 'orange'"},
                                    "size": {"type": "string", "description": "The size of the orange"}
                                },
                                "propertyOrdering": ["type", "size"],
                                "required": ["type", "size"]
                            }
                        ]
                    }
                }
            }
        });
        assert_rows(vec![
            // google-tool-schema.test.ts: gemini side of "collapses mixed-type anyOf".
            (
                "gemini mixed anyOf lines",
                json!({"anyOf": [{"type": "array", "items": {"type": "string"}}, {"type": "string"}, {"type": "null"}]}),
                json!({"nullable": true, "anyOf": [{"type": "array", "items": {"type": "string"}}, {"type": "string"}]}),
            ),
            // google-tool-schema.test.ts: gemini side of "preserves nullable unions".
            (
                "gemini nullable enum union",
                json!({"anyOf": [{"enum": ["A", "B"]}, {"type": "null"}]}),
                json!({"nullable": true, "enum": ["A", "B"], "type": "string"}),
            ),
            (
                "keeps google sanitizer behavior for non-claude schema path",
                json!({"type": "object", "properties": {"value": {"type": ["string", "null"]}}}),
                json!({"type": "object", "properties": {"value": {"type": "string", "nullable": true}}}),
            ),
            (
                "splits a multi-type array into typed anyOf branches",
                json!({"type": ["string", "array", "null"], "items": {"type": "number"}, "description": "Ref or point."}),
                json!({
                    "description": "Ref or point.",
                    "nullable": true,
                    "anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "number"}}]
                }),
            ),
            (
                "normalizes schemas for gemini models (additionalProperties stripped)",
                json!({"type": "object", "properties": {"value": {"type": "string"}}, "additionalProperties": false}),
                json!({"type": "object", "properties": {"value": {"type": "string"}}}),
            ),
            (
                "infers enum type through convertTools for gemini",
                json!({"type": "object", "properties": {"action": {"enum": ["definition", "references", "code_actions"]}}, "required": ["action"]}),
                json!({
                    "type": "object",
                    "properties": {"action": {"type": "string", "enum": ["definition", "references", "code_actions"]}},
                    "required": ["action"]
                }),
            ),
            (
                "leaves anyOf alone when no variant has type null",
                json!({"anyOf": [{"type": "integer"}, {"type": "number"}], "default": "null", "title": "Total Area Sq Mi"}),
                json!({"anyOf": [{"type": "integer"}, {"type": "number"}], "default": "null", "title": "Total Area Sq Mi"}),
            ),
            (
                "preserves multi-variant anyOf without any null variant (any_of renamed)",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "title": "Name"},
                        "restaurants_per_capita": {"any_of": [{"type": "integer"}, {"type": "number"}], "title": "Restaurants Per Capita"}
                    },
                    "required": ["name", "restaurants_per_capita"]
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "title": "Name"},
                        "restaurants_per_capita": {"anyOf": [{"type": "integer"}, {"type": "number"}], "title": "Restaurants Per Capita"}
                    },
                    "required": ["name", "restaurants_per_capita"],
                    "propertyOrdering": ["name", "restaurants_per_capita"]
                }),
            ),
            (
                "leaves already-camelCased complex schemas unchanged",
                dict_schema.clone(),
                dict_schema,
            ),
            (
                "converts const to a singleton enum",
                json!({"type": "string", "const": "FOO"}),
                json!({"type": "string", "enum": ["FOO"]}),
            ),
            (
                "omits a non-string const enum while preserving its type",
                json!({"type": "integer", "const": 123}),
                json!({"type": "integer"}),
            ),
            (
                "drops negations whose non-string enums cannot be represented",
                json!({"not": {"enum": [1]}}),
                json!({}),
            ),
            (
                "drops negations with nested non-string enums",
                json!({"not": {"type": "object", "properties": {"value": {"enum": [1]}}}}),
                json!({}),
            ),
            (
                "drops negations containing snake-case combiners with non-string enums",
                json!({"not": {"any_of": [{"const": 1}]}}),
                json!({}),
            ),
            (
                "[sorted] propagates auto propertyOrdering into inlined $defs targets",
                json!({
                    "$ref": "#/$defs/Foo",
                    "$defs": {"Foo": {"type": "object", "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}}}}
                }),
                json!({
                    "type": "object",
                    "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}},
                    "propertyOrdering": ["bar", "foo"]
                }),
            ),
            (
                "[sorted] propagates auto propertyOrdering into array items",
                json!({"type": "array", "items": {"type": "object", "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}}}}),
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}},
                        "propertyOrdering": ["bar", "foo"]
                    }
                }),
            ),
            (
                "[sorted] propagates auto propertyOrdering into nested properties",
                json!({
                    "type": "object",
                    "properties": {
                        "xyz": {"type": "object", "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}}},
                        "abc": {"type": "string"}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "xyz": {
                            "type": "object",
                            "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}},
                            "propertyOrdering": ["bar", "foo"]
                        },
                        "abc": {"type": "string"}
                    },
                    "propertyOrdering": ["abc", "xyz"]
                }),
            ),
            (
                "[sorted] propagates auto propertyOrdering into anyOf variants",
                json!({
                    "anyOf": [
                        {"type": "object", "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}}},
                        {"type": "string"}
                    ]
                }),
                json!({
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}},
                            "propertyOrdering": ["bar", "foo"]
                        },
                        {"type": "string"}
                    ]
                }),
            ),
            (
                "breaks $ref cycles by emitting an empty schema at the recursion point",
                json!({
                    "type": "object",
                    "properties": {"recursive": {"$ref": "#/$defs/RecursiveObject"}},
                    "$defs": {
                        "RecursiveObject": {"type": "object", "properties": {"self": {"$ref": "#/$defs/RecursiveObject"}}}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {"recursive": {"type": "object", "properties": {"self": {}}}}
                }),
            ),
            (
                "does not overwrite an existing propertyOrdering",
                json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}, "code": {"type": "string"}, "symbol": {"type": "string"}},
                    "propertyOrdering": ["code", "symbol", "name"]
                }),
                json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}, "code": {"type": "string"}, "symbol": {"type": "string"}},
                    "propertyOrdering": ["code", "symbol", "name"]
                }),
            ),
            (
                "normalizes snake_case schema field names to camelCase",
                json!({
                    "type": "object",
                    "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}},
                    "property_ordering": ["bar", "foo"]
                }),
                json!({
                    "type": "object",
                    "properties": {"foo": {"type": "string"}, "bar": {"type": "string"}},
                    "propertyOrdering": ["bar", "foo"]
                }),
            ),
            (
                "lets snake_case overwrite an existing camelCase entry on collision",
                json!({"anyOf": [{"type": "string"}], "any_of": [{"type": "integer"}, {"type": "number"}]}),
                json!({"anyOf": [{"type": "integer"}, {"type": "number"}]}),
            ),
            (
                "rewrites a bare {type:'null'} schema as {nullable:true}",
                json!({"type": "null"}),
                json!({"nullable": true}),
            ),
            (
                "flattens anyOf:[X, {type:'null'}] into X + nullable",
                json!({"anyOf": [{"type": "string", "title": "Name"}, {"type": "null"}]}),
                json!({"type": "string", "title": "Name", "nullable": true}),
            ),
            // schema-normalization.test.ts "normalizeSchemaForGoogle".
            (
                "[sorted] preserves string enums and removes enums Google cannot represent",
                json!({
                    "type": "object",
                    "properties": {
                        "valid": {"type": "string", "enum": ["draft", "published"]},
                        "numeric": {"type": "number", "enum": [1, 2]},
                        "mixed": {"enum": ["draft", 1]}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "valid": {"type": "string", "enum": ["draft", "published"]},
                        "numeric": {"type": "number"},
                        "mixed": {}
                    },
                    "propertyOrdering": ["mixed", "numeric", "valid"]
                }),
            ),
            (
                "preserves enum keys inside object-valued defaults",
                json!({"type": "object", "default": {"enum": [1], "value": 2}}),
                json!({"type": "object", "default": {"enum": [1], "value": 2}, "properties": {}}),
            ),
            (
                "sets object type while removing an object-valued enum converted from const",
                json!({"const": {"a": 1}}),
                json!({"type": "object", "properties": {}}),
            ),
            (
                "removes an object-valued enum after deduplicating a deep-equal const",
                json!({"type": "object", "enum": [{"a": 1}], "const": {"a": 1}}),
                json!({"type": "object", "properties": {}}),
            ),
            (
                "removes an enum when const variants span multiple primitive types",
                json!({
                    "anyOf": [
                        {"const": "A", "type": "string"},
                        {"const": 1, "type": "number"},
                        {"const": true, "type": "boolean"}
                    ]
                }),
                json!({}),
            ),
            (
                "collapses inferred null type to nullable while removing its enum",
                json!({"const": null}),
                json!({"nullable": true}),
            ),
            (
                "coerces a boolean subschema literally named additionalProperties inside properties",
                json!({"type": "object", "properties": {"additionalProperties": false, "name": {"type": "string"}}}),
                json!({
                    "type": "object",
                    "properties": {"additionalProperties": {"not": {}}, "name": {"type": "string"}},
                    "propertyOrdering": ["additionalProperties", "name"]
                }),
            ),
            (
                "coerces a boolean subschema for a single property literally named additionalProperties",
                json!({"type": "object", "properties": {"additionalProperties": false}, "required": ["additionalProperties"]}),
                json!({
                    "type": "object",
                    "properties": {"additionalProperties": {"not": {}}},
                    "required": ["additionalProperties"]
                }),
            ),
            (
                "[sorted] coerces boolean subschemas to object equivalents on the Google wire",
                json!({"type": "object", "properties": {"propertyValue": true, "attributeValue": false}}),
                json!({
                    "type": "object",
                    "properties": {"propertyValue": {}, "attributeValue": {"not": {}}},
                    "propertyOrdering": ["attributeValue", "propertyValue"]
                }),
            ),
            ("root true coerces to {}", json!(true), json!({})),
            (
                "root false coerces to {not: {}}",
                json!(false),
                json!({"not": {}}),
            ),
            (
                "anyOf boolean branch coerces",
                json!({"anyOf": [true, {"type": "string"}]}),
                json!({"anyOf": [{}, {"type": "string"}]}),
            ),
            (
                "strips the MCP x-mcp-header transport annotation",
                json!({
                    "type": "object",
                    "properties": {
                        "owner": {"type": "string", "x-mcp-header": "owner"},
                        "repo": {"type": "string", "x-mcp-header": "repo"}
                    },
                    "required": ["owner", "repo"]
                }),
                json!({
                    "type": "object",
                    "properties": {"owner": {"type": "string"}, "repo": {"type": "string"}},
                    "required": ["owner", "repo"],
                    "propertyOrdering": ["owner", "repo"]
                }),
            ),
            (
                "keeps a property literally named x-mcp-header",
                json!({"type": "object", "properties": {"x-mcp-header": {"type": "string"}}}),
                json!({"type": "object", "properties": {"x-mcp-header": {"type": "string"}}}),
            ),
            (
                "strips draft-2019 conditional keywords the OpenAPI-style wire cannot model",
                json!({
                    "type": "object",
                    "properties": {"propertyValue": true},
                    "dependentSchemas": {"hasFoo": true},
                    "dependentRequired": {"hasFoo": ["propertyValue"]}
                }),
                json!({"type": "object", "properties": {"propertyValue": {}}}),
            ),
            (
                "[sorted] strips uniqueItems and unsupported array/content validation keywords",
                json!({
                    "type": "object",
                    "properties": {
                        "tags": {"type": "array", "items": {"type": "string"}, "uniqueItems": true, "description": "List of tags"},
                        "data": {"type": "string", "contentEncoding": "base64", "contentMediaType": "image/png"}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "tags": {"type": "array", "items": {"type": "string"}, "description": "List of tags\n\n{uniqueItems: true}"},
                        "data": {"type": "string"}
                    },
                    "propertyOrdering": ["data", "tags"]
                }),
            ),
            (
                "inlines local $ref / $defs entries for Google compatibility",
                json!({
                    "type": "object",
                    "properties": {"user": {"$ref": "#/$defs/User"}},
                    "required": ["user"],
                    "$defs": {
                        "User": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "user": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}
                    },
                    "required": ["user"]
                }),
            ),
            (
                "[sorted] lifts stripped validation keywords into description",
                json!({"type": "string", "pattern": "^\\d+$", "minLength": 1, "maxLength": 8, "description": "ID"}),
                json!({"type": "string", "description": "ID\n\n{maxLength: 8, minLength: 1, pattern: \"^\\\\d+$\"}"}),
            ),
            // schema-normalization.test.ts "DAG-shared subtree normalization"
            // (a serde tree holds two copies; omp's circular-schema case cannot
            // be expressed without cycles).
            (
                "normalizes a subschema reused across two properties",
                json!({
                    "type": "object",
                    "properties": {
                        "a": {"type": "string", "description": "shared leaf"},
                        "b": {"type": "string", "description": "shared leaf"}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "a": {"type": "string", "description": "shared leaf"},
                        "b": {"type": "string", "description": "shared leaf"}
                    },
                    "propertyOrdering": ["a", "b"]
                }),
            ),
            (
                "does not throw on a frozen input schema",
                json!({"type": "object", "properties": {"x": {"type": "number"}, "y": {"type": "number"}}}),
                json!({
                    "type": "object",
                    "properties": {"x": {"type": "number"}, "y": {"type": "number"}},
                    "propertyOrdering": ["x", "y"]
                }),
            ),
            // schema-compatibility.test.ts "validates Google-compatible schemas after sanitization".
            (
                "google-compatible schema after sanitization",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "pattern": {"type": "string", "description": "property name that matches schema keyword"},
                        "status": {"type": ["string", "null"], "const": "active"}
                    },
                    "required": ["status"]
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "property name that matches schema keyword"},
                        "status": {"type": "string", "nullable": true, "enum": ["active"]}
                    },
                    "required": ["status"],
                    "propertyOrdering": ["pattern", "status"]
                }),
            ),
        ]);
    }

    /// Rows derived by reading the walker for branches omp does not test
    /// through `normalizeSchemaForGoogle`; each comment names the rule.
    #[test]
    fn derived_rows() {
        assert_rows(vec![
            // draft.ts:265-268,290-293: `definitions` → `$defs`, ref rewritten, then inlined.
            (
                "draft-07 definitions and $ref",
                json!({"type": "object", "properties": {"a": {"$ref": "#/definitions/A"}}, "definitions": {"A": {"type": "string"}}}),
                json!({"type": "object", "properties": {"a": {"type": "string"}}}),
            ),
            // local-ref.ts: percent-encoded fragment decodes to `/definitions/A`.
            (
                "percent-encoded draft-07 ref",
                json!({"type": "object", "properties": {"a": {"$ref": "#%2Fdefinitions%2FA"}}, "definitions": {"A": {"type": "string"}}}),
                json!({"type": "object", "properties": {"a": {"type": "string"}}}),
            ),
            // draft.ts:321-325 makeNullable: scalar type → ["string","null"] → scalarized + nullable.
            (
                "openapi nullable on typed schema",
                json!({"type": "string", "nullable": true}),
                json!({"type": "string", "nullable": true}),
            ),
            // draft.ts:186 makeNullable wraps untyped schema; preHandleNullFields unwraps it.
            (
                "openapi nullable on bare enum",
                json!({"nullable": true, "enum": ["a"]}),
                json!({"nullable": true, "enum": ["a"], "type": "string"}),
            ),
            // draft.ts:303-314 tuple items → prefixItems (stripped), additionalItems → items.
            (
                "draft-07 tuple with additionalItems schema",
                json!({"type": "array", "items": [{"type": "string"}, {"type": "integer"}], "additionalItems": {"type": "number"}}),
                json!({"type": "array", "items": {"type": "number"}}),
            ),
            // draft.ts:311-313 absent additionalItems deletes `items`.
            (
                "draft-07 tuple without additionalItems",
                json!({"type": "array", "items": [{"type": "string"}]}),
                json!({"type": "array"}),
            ),
            // draft.ts:140-152 dependencies → dependentRequired, then stripped as unsupported.
            (
                "draft-07 dependencies",
                json!({"type": "object", "properties": {"a": {"type": "string"}}, "dependencies": {"a": ["b"]}}),
                json!({"type": "object", "properties": {"a": {"type": "string"}}}),
            ),
            // draft.ts:285-288 `$schema` upgraded, then stripped (fields.ts:17).
            (
                "draft-07 $schema",
                json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "string"}),
                json!({"type": "string"}),
            ),
            // dereference.ts:91-100 sibling keywords override the inlined definition.
            (
                "$ref sibling keywords",
                json!({
                    "type": "object",
                    "properties": {"a": {"$ref": "#/$defs/A", "description": "field"}},
                    "$defs": {"A": {"type": "string", "description": "def"}}
                }),
                json!({"type": "object", "properties": {"a": {"type": "string", "description": "field"}}}),
            ),
            // dereference.ts:78 external ref untouched, then `$ref` stripped (fields.ts:20).
            (
                "external $ref",
                json!({"type": "object", "properties": {"a": {"$ref": "https://example.com/s.json#/A"}}, "$defs": {}}),
                json!({"type": "object", "properties": {"a": {}}}),
            ),
            // normalize.ts:228-249 snake_case `additional_properties` renamed, then stripped.
            (
                "snake_case additional_properties",
                json!({"type": "object", "properties": {"a": {"type": "string"}}, "additional_properties": false}),
                json!({"type": "object", "properties": {"a": {"type": "string"}}}),
            ),
            // normalize.ts:228-249 snake_case `prefix_items` renamed, then stripped.
            (
                "snake_case prefix_items",
                json!({"type": "array", "prefix_items": [{"type": "string"}], "items": {"type": "string"}}),
                json!({"type": "array", "items": {"type": "string"}}),
            ),
            // spill.ts:41-42 spill without an existing description (sorted key order).
            (
                "spill without description",
                json!({"type": "integer", "minimum": 0, "maximum": 10, "format": "int32"}),
                json!({"type": "integer", "description": "{format: \"int32\", maximum: 10, minimum: 0}"}),
            ),
            // spill.ts:5-7 JSON.stringify renders 1.0 as `1`.
            (
                "spill renders JavaScript numbers",
                json!({"type": "number", "minimum": 1.0, "multipleOf": 0.5}),
                json!({"type": "number", "description": "{minimum: 1, multipleOf: 0.5}"}),
            ),
            // fields.ts:63-66 annotation keywords strip silently; `examples` is liftable.
            (
                "unsupported field strip",
                json!({"type": "string", "readOnly": true, "deprecated": true, "$comment": "c", "examples": ["x"]}),
                json!({"type": "string", "description": "{examples: [\"x\"]}"}),
            ),
            // normalize.ts:553-565 bare enum of one JSON type gets that type.
            (
                "bare string enum type inference",
                json!({"enum": ["a", "b"]}),
                json!({"type": "string", "enum": ["a", "b"]}),
            ),
            // normalize.ts:553-565 + 625-629 numeric bare enum: type kept, enum dropped.
            (
                "bare numeric enum",
                json!({"enum": [1, 2]}),
                json!({"type": "number"}),
            ),
            // normalize.ts:556 no inference beside an anyOf.
            (
                "bare enum beside anyOf",
                json!({"enum": ["a"], "anyOf": [{"type": "string"}]}),
                json!({"enum": ["a"], "anyOf": [{"type": "string"}]}),
            ),
            // normalize.ts:625-629 boolean enum dropped; type inferred first.
            (
                "boolean enum",
                json!({"enum": [true, false]}),
                json!({"type": "boolean"}),
            ),
            // normalize.ts:627 empty enum is not a string enum.
            (
                "empty enum dropped",
                json!({"type": "string", "enum": []}),
                json!({"type": "string"}),
            ),
            // normalize.ts:464-471 const union with null → single type + nullable; null enum dropped.
            (
                "const union with null",
                json!({"anyOf": [{"const": "a"}, {"const": null}]}),
                json!({"type": "string", "nullable": true}),
            ),
            // normalize.ts:448-456 oneOf consts with one explicit type; siblings kept.
            (
                "const oneOf with explicit type",
                json!({"oneOf": [{"const": "a", "type": "string"}, {"const": "b", "type": "string"}], "description": "d"}),
                json!({"enum": ["a", "b"], "type": "string", "description": "d"}),
            ),
            // normalize.ts:478-481,500 const-union branch spills stripped siblings.
            (
                "const union spills stripped siblings",
                json!({"anyOf": [{"const": "a"}], "format": "x"}),
                json!({"enum": ["a"], "type": "string", "description": "{format: \"x\"}"}),
            ),
            // normalize.ts:535-543 scalarization drops keywords of other types (`items`).
            (
                "type array scalarization drops foreign keywords",
                json!({"type": ["integer", "null"], "items": {"type": "string"}, "minimum": 1}),
                json!({"type": "integer", "nullable": true, "description": "{minimum: 1}"}),
            ),
            // normalize.ts:541 + 567-570 `["null"]` scalarizes to null, then collapses.
            (
                "type array of only null",
                json!({"type": ["null"]}),
                json!({"nullable": true}),
            ),
            // normalize.ts:541 empty type array leaves no type.
            ("empty type array", json!({"type": []}), json!({})),
            // normalize.ts:314-331 split routes type-specific keywords to their branch.
            (
                "split object/string type array",
                json!({"type": ["object", "string"], "properties": {"a": {"type": "string"}}, "required": ["a"], "maxLength": 3}),
                json!({
                    "anyOf": [
                        {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                        {"type": "string", "description": "{maxLength: 3}"}
                    ]
                }),
            ),
            // normalize.ts:284-285 anyOf of only null collapses to nullable.
            (
                "anyOf of only null",
                json!({"anyOf": [{"type": "null"}], "description": "d"}),
                json!({"nullable": true, "description": "d"}),
            ),
            // normalize.ts:586-588 ensureObjectProperties.
            (
                "object without properties",
                json!({"type": "object"}),
                json!({"type": "object", "properties": {}}),
            ),
            // normalize.ts:404-412 `items: false` is a subschema slot.
            (
                "boolean items subschema",
                json!({"type": "array", "items": false}),
                json!({"type": "array", "items": {"not": {}}}),
            ),
            // normalize.ts:404-412 `not: true` coerces.
            (
                "boolean not subschema",
                json!({"not": true}),
                json!({"not": {}}),
            ),
            // normalize.ts:517-524 negated non-string const dropped.
            (
                "negated non-string const",
                json!({"type": "string", "not": {"const": 1}}),
                json!({"type": "string"}),
            ),
            // normalize.ts:517-524 representable negation kept and normalized.
            (
                "representable negation",
                json!({"type": "string", "not": {"enum": ["x"]}}),
                json!({"type": "string", "not": {"enum": ["x"], "type": "string"}}),
            ),
            // normalize.ts:413-415 non-schema roots pass through.
            ("string root", json!("x"), json!("x")),
            ("null root", Value::Null, Value::Null),
        ]);
    }

    /// schema-normalization.test.ts `upgradeJsonSchemaTo202012`.
    #[test]
    fn upgrade_rows() {
        let schema = json!({
            "type": "object",
            "properties": {
                "definitions": {"type": "string"},
                "tuple": {"type": "array", "items": [{"type": "string"}, {"type": "integer"}], "additionalItems": false},
                "gated": {"type": "object", "dependencies": {"a": ["b"], "c": {"required": ["d"]}}}
            },
            "definitions": {"Ref": {"type": "string"}}
        });
        assert!(schema_needs_draft_202012_upgrade(&schema));
        assert_eq!(
            upgrade_json_schema_to_202012(&schema).into_owned(),
            json!({
                "type": "object",
                "properties": {
                    "definitions": {"type": "string"},
                    "tuple": {"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}], "items": false},
                    "gated": {"type": "object", "dependentRequired": {"a": ["b"]}, "dependentSchemas": {"c": {"required": ["d"]}}}
                },
                "$defs": {"Ref": {"type": "string"}}
            })
        );

        let unchanged = json!({"type": "object", "properties": {"name": {"type": "string"}}});
        assert!(!schema_needs_draft_202012_upgrade(&unchanged));
        assert!(matches!(
            upgrade_json_schema_to_202012(&unchanged),
            Cow::Borrowed(_)
        ));
    }

    /// schema-dereference.test.ts.
    #[test]
    fn dereference_rows() {
        let deref = |schema: Value| dereference_json_schema(&schema).into_owned();

        for referenced in [false, true] {
            let literal = json!({
                "$ref": "#/$defs/Text",
                "$defs": {"keep": "literal"},
                "definitions": {"keep": "literal"},
                "properties": {"nested": {"$ref": "#/$defs/Text"}}
            });
            let mut value = json!({
                "const": literal,
                "enum": [literal],
                "default": literal,
                "examples": [literal],
                "x-payload": literal
            });
            let mut expected_value = value.clone();
            expected_value["type"] = json!("object");
            if referenced {
                value["$ref"] = json!("#/$defs/Value");
            } else {
                value["type"] = json!("object");
            }
            let schema = json!({
                "$defs": {"Text": {"type": "string"}, "Value": {"type": "object"}},
                "properties": {"value": value}
            });
            assert_eq!(
                deref(schema),
                json!({"properties": {"value": expected_value}}),
                "literal JSON preserved (referenced: {referenced})"
            );

            let reference = json!({"$ref": "#/$defs/Text"});
            let text = json!({"type": "string"});
            let mut base = json!({"$defs": {"Text": text, "Value": {"type": "object"}}});
            if referenced {
                base["$ref"] = json!("#/$defs/Value");
            } else {
                base["type"] = json!("object");
            }
            let named_refs = json!({"$ref": reference, "$defs": reference, "definitions": reference, "const": reference});
            let named_schemas =
                json!({"$ref": text, "$defs": text, "definitions": text, "const": text});
            let with = |keyword: &str, value: Value| {
                let mut schema = base.clone();
                schema[keyword] = value;
                schema
            };
            for keyword in [
                "properties",
                "patternProperties",
                "dependencies",
                "dependentSchemas",
            ] {
                let mut expected = json!({"type": "object"});
                expected[keyword] = named_schemas.clone();
                assert_eq!(
                    deref(with(keyword, named_refs.clone())),
                    expected,
                    "{keyword}"
                );
            }
            for keyword in ["anyOf", "oneOf", "allOf", "prefixItems", "items"] {
                let mut expected = json!({"type": "object"});
                expected[keyword] = json!([text, false]);
                assert_eq!(
                    deref(with(keyword, json!([reference, false]))),
                    expected,
                    "{keyword}"
                );
            }
            for keyword in DEREF_SCHEMA_VALUE_KEYS {
                let mut expected = json!({"type": "object"});
                expected[*keyword] = text.clone();
                assert_eq!(
                    deref(with(keyword, reference.clone())),
                    expected,
                    "{keyword}"
                );
            }
            assert_eq!(
                deref(with("dependencies", json!({"field": ["$ref", "$defs"]}))),
                json!({"type": "object", "dependencies": {"field": ["$ref", "$defs"]}})
            );
        }

        let no_defs = json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]});
        assert!(matches!(
            dereference_json_schema(&no_defs),
            Cow::Borrowed(_)
        ));

        let rows = vec![
            (
                "inlines a simple $ref from $defs",
                json!({
                    "type": "object",
                    "properties": {"anchor": {"$ref": "#/$defs/Anchor"}},
                    "$defs": {"Anchor": {"type": "object", "properties": {"path": {"type": "string"}, "line": {"type": "integer", "minimum": 0}}}}
                }),
                json!({
                    "type": "object",
                    "properties": {"anchor": {"type": "object", "properties": {"path": {"type": "string"}, "line": {"type": "integer", "minimum": 0}}}}
                }),
            ),
            (
                "inlines $ref from definitions (legacy keyword)",
                json!({
                    "type": "object",
                    "properties": {"item": {"$ref": "#/definitions/Item"}},
                    "definitions": {"Item": {"type": "string", "enum": ["a", "b"]}}
                }),
                json!({"type": "object", "properties": {"item": {"type": "string", "enum": ["a", "b"]}}}),
            ),
            (
                "inlines nested $ref inside arrays (items, anyOf, oneOf)",
                json!({
                    "type": "object",
                    "properties": {
                        "anchors": {"type": "array", "items": {"$ref": "#/$defs/SourceAnchorInput"}},
                        "value": {"anyOf": [{"$ref": "#/$defs/Str"}, {"type": "null"}]}
                    },
                    "$defs": {
                        "SourceAnchorInput": {
                            "type": "object",
                            "properties": {
                                "anchor_type": {"type": "string", "enum": ["file", "symbol", "pattern"]},
                                "path": {"type": "string"}
                            },
                            "required": ["anchor_type", "path"]
                        },
                        "Str": {"type": "string"}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "anchors": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "anchor_type": {"type": "string", "enum": ["file", "symbol", "pattern"]},
                                    "path": {"type": "string"}
                                },
                                "required": ["anchor_type", "path"]
                            }
                        },
                        "value": {"anyOf": [{"type": "string"}, {"type": "null"}]}
                    }
                }),
            ),
            (
                "handles $ref inside a definition pointing to another definition",
                json!({
                    "type": "object",
                    "properties": {"wrapper": {"$ref": "#/$defs/Wrapper"}},
                    "$defs": {
                        "Inner": {"type": "integer", "minimum": 0},
                        "Wrapper": {"type": "object", "properties": {"value": {"$ref": "#/$defs/Inner"}}}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {"wrapper": {"type": "object", "properties": {"value": {"type": "integer", "minimum": 0}}}}
                }),
            ),
            (
                "breaks circular $ref with empty object",
                json!({
                    "type": "object",
                    "properties": {"node": {"$ref": "#/$defs/TreeNode"}},
                    "$defs": {
                        "TreeNode": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "children": {"type": "array", "items": {"$ref": "#/$defs/TreeNode"}}
                            }
                        }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "node": {
                            "type": "object",
                            "properties": {"name": {"type": "string"}, "children": {"type": "array", "items": {}}}
                        }
                    }
                }),
            ),
            (
                "leaves external $ref untouched",
                json!({"type": "object", "properties": {"ext": {"$ref": "https://example.com/schema.json#/Foo"}}, "$defs": {}}),
                json!({"type": "object", "properties": {"ext": {"$ref": "https://example.com/schema.json#/Foo"}}}),
            ),
            (
                "preserves sibling keywords alongside $ref",
                json!({
                    "type": "object",
                    "properties": {
                        "anchor_type": {"$ref": "#/$defs/AnchorType", "description": "Field-level description", "default": "file"}
                    },
                    "$defs": {
                        "AnchorType": {"type": "string", "enum": ["file", "symbol", "pattern"], "description": "Type-level description"}
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "anchor_type": {
                            "type": "string",
                            "enum": ["file", "symbol", "pattern"],
                            "description": "Field-level description",
                            "default": "file"
                        }
                    }
                }),
            ),
            (
                "handles multiple properties referencing the same $def",
                json!({
                    "type": "object",
                    "properties": {"a": {"$ref": "#/$defs/Shared"}, "b": {"$ref": "#/$defs/Shared"}},
                    "$defs": {"Shared": {"type": "string", "maxLength": 100}}
                }),
                json!({
                    "type": "object",
                    "properties": {"a": {"type": "string", "maxLength": 100}, "b": {"type": "string", "maxLength": 100}}
                }),
            ),
        ];
        for (name, input, expected) in rows {
            assert_eq!(deref(input), expected, "row: {name}");
        }
    }

    #[test]
    fn local_ref_pointer_round_trip() {
        assert_eq!(
            parse_local_ref_pointer("#/a~1b/c~0d"),
            Some(vec!["a/b".to_owned(), "c~d".to_owned()])
        );
        assert_eq!(parse_local_ref_pointer("#"), Some(Vec::new()));
        assert_eq!(parse_local_ref_pointer("#bad"), None);
        assert_eq!(parse_local_ref_pointer("#%E0%A4%A"), None);
        assert_eq!(parse_local_ref_pointer("https://x/#/a"), None);
        assert_eq!(
            format_local_ref_pointer(&["$defs".to_owned(), "a/b c".to_owned()]),
            "#/$defs/a~1b%20c"
        );
        assert_eq!(convert_ref("#/definitions/Foo"), "#/$defs/Foo");
        assert_eq!(convert_ref("#/other/Foo"), "#/other/Foo");
    }
}
