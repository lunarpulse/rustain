//! K19: the Gemini CLI tool-schema degrade residual through
//! `build_provider_for_config` → `stream_completion` at a loopback stub. A `not`
//! whose subtree a dereference bound cut to `{}` is not sent (it would reject
//! every value), an undegraded `not` is, and the warning names the bound that
//! fired once per distinct schema.

use crate::conformance_20_1d4_gemini_cli::auth::config;
use crate::conformance_20_1d4_gemini_cli::replay::Capture;
use crate::conformance_20_1d4b_gemini_cli_discovery::common::{
    MODEL, PROVIDER, STREAM, World, hits, mount_stream, options, user,
};
use futures::StreamExt;
use rustain::domain::models::{StreamChunk, ToolDefinition};
use rustain::domain::ports::StreamingProvider;
use rustain::infrastructure::provider_factory::build_provider_for_config;
use serde_json::{Map, Value, json};
use serial_test::serial;
use std::{sync::Arc, time::Duration};

const BYTES_WARNING: &str =
    "Gemini CLI tool schema exceeded the $ref inlining byte budget; the excess is sent as {}";
const DEPTH_WARNING: &str =
    "Gemini CLI tool schema exceeded the nesting depth bound; the excess is sent as {}";

/// An undegraded negation beside the degraded one (positive control).
fn kept() -> Value {
    json!({"type": "string", "not": {"enum": ["k19-forbidden"]}})
}

/// `levels` nested `properties` without definitions: past the depth bound.
fn deep_properties(levels: usize) -> Value {
    let mut node = json!({"type": "string"});
    for _ in 0..levels {
        node = json!({"type": "object", "properties": {"k19": node}});
    }
    node
}

/// Definitions each referencing the next twice with 512-byte literal leaves:
/// about 9 MB inlined naively, past the 1 MiB byte budget.
fn doubling_defs(levels: usize) -> Map<String, Value> {
    let literal = "K".repeat(512);
    let mut defs = Map::new();
    for level in 0..levels {
        let next = json!({"$ref": format!("#/$defs/d{}", level + 1)});
        defs.insert(
            format!("d{level}"),
            json!({"type": "object", "description": literal, "enum": [literal],
                   "properties": {"a": next.clone(), "b": next}}),
        );
    }
    defs.insert(format!("d{levels}"), json!({"type": "string"}));
    defs
}

/// The depth bound fires inside a `not`, nested once more under a `not`.
fn depth_schema() -> Value {
    json!({"type": "object", "properties": {
        "guarded": {"not": {"type": "object", "properties": {
            "inner": {"not": deep_properties(80)}
        }}},
        "kept": kept()
    }})
}

/// The byte budget fires inside a `not` (`guarded` walks before `kept`).
fn bytes_schema() -> Value {
    json!({"type": "object", "properties": {
        "guarded": {"not": {"$ref": "#/$defs/d0"}},
        "kept": kept()
    }, "$defs": doubling_defs(12)})
}

fn tool(name: &str, input_schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("{name} fixture"),
        input_schema,
        parallel_safe: false,
    }
}

async fn stream(provider: &Arc<dyn StreamingProvider>, tools: Vec<ToolDefinition>) {
    let mut opts = options(MODEL);
    opts.tools = tools;
    let chunks: Vec<StreamChunk> = tokio::time::timeout(Duration::from_secs(30), async {
        provider
            .stream_completion(vec![user("hi")], opts)
            .await
            .expect("the request is sent")
            .collect::<Vec<_>>()
            .await
    })
    .await
    .expect("stream finished");
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Text { content, .. } if content == "ok")),
        "{chunks:?}"
    );
}

/// The `parametersJsonSchema` of every tool in the last stream POST, by name.
async fn sent_schemas(world: &World) -> Map<String, Value> {
    let requests = world.requests().await;
    let post = *hits(&requests, STREAM).last().expect("a stream POST");
    let body: Value = serde_json::from_slice(&post.body).expect("JSON body");
    body["request"]["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("function declarations")
        .iter()
        .map(|declaration| {
            (
                declaration["name"].as_str().unwrap().to_owned(),
                declaration["parametersJsonSchema"].clone(),
            )
        })
        .collect()
}

/// The JSON pointer of every `not` keyword under `value`.
fn not_paths(value: &Value, at: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map {
                let here = format!("{at}/{key}");
                if key == "not" {
                    out.push(here.clone());
                }
                not_paths(entry, &here, out);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                not_paths(item, &format!("{at}/{index}"), out);
            }
        }
        _ => {}
    }
}

fn logs(capture: &Capture) -> String {
    String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
}

#[tokio::test]
#[serial]
async fn k19_a_degraded_not_is_dropped_and_its_budget_warned_once_per_schema() {
    let world = World::signed_in().await;
    mount_stream(&world.server).await;
    let provider =
        build_provider_for_config(PROVIDER, &config(None)).expect("built from the stored login");
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    // Thread-scoped: the current-thread runtime encodes on this thread.
    let trace_guard = tracing::subscriber::set_default(subscriber);

    let tools = || {
        vec![
            tool("k19_depth", depth_schema()),
            tool("k19_bytes", bytes_schema()),
        ]
    };
    stream(&provider, tools()).await;
    let first = sent_schemas(&world).await;
    for name in ["k19_depth", "k19_bytes"] {
        let schema = &first[name];
        // No `not` above the degraded node: the whole guarded chain is gone…
        assert_eq!(
            schema["properties"]["guarded"].get("not"),
            None,
            "{name}: {schema}"
        );
        // …and the undegraded negation is sent (positive control).
        let mut paths = Vec::new();
        not_paths(schema, "", &mut paths);
        assert_eq!(paths, ["/properties/kept/not"], "{name}: {schema}");
        assert!(
            schema["properties"]["kept"]["not"]["enum"]
                .as_array()
                .is_some_and(|values| values.contains(&json!("k19-forbidden"))),
            "{name}: {schema}"
        );
    }
    let after_first = logs(&capture);
    assert_eq!(
        after_first.matches(BYTES_WARNING).count(),
        1,
        "{after_first}"
    );
    assert_eq!(
        after_first.matches(DEPTH_WARNING).count(),
        1,
        "{after_first}"
    );

    // The same schemas again: the same body, and no further warning.
    stream(&provider, tools()).await;
    assert_eq!(sent_schemas(&world).await, first);
    drop(trace_guard);
    let after_second = logs(&capture);
    assert_eq!(
        after_second
            .matches("Gemini CLI tool schema exceeded")
            .count(),
        2,
        "{after_second}"
    );
}
