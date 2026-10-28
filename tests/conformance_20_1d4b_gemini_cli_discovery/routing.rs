//! K4: `GeminiCliAdapter::stream_completion` through `build_provider_for_config`
//! sends a collapsed logical id's no-effort wire id and thinking; bundled ids are
//! unchanged. Plus the inherited DF-20-1D4A-SCHEMA-DEREF-BYTE-BUDGET keystone
//! on the same production request path.
use super::common::*;
use crate::conformance_20_1d4_gemini_cli::replay::Capture;
use futures::StreamExt;
use rustain::adapters::model_catalog_cache::ModelCatalogCache;
use rustain::domain::models::{StreamChunk, ToolDefinition};
use rustain::domain::ports::StreamingProvider;
use rustain::infrastructure::model_discovery::refresh_target;
use rustain::infrastructure::provider_factory::build_provider_for_config;
use rustain::infrastructure::startup::init_provider_layer;
use serde_json::{Map, Value, json};
use serial_test::serial;
use std::{sync::Arc, time::Duration};
use wiremock::Request;

async fn stream(provider: &Arc<dyn StreamingProvider>, model: &str, tools: Vec<ToolDefinition>) {
    let mut opts = options(model);
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

/// The last stream POST: (raw body, `request.generationConfig`).
async fn last_post(world: &World) -> (Request, String, Value) {
    let requests = world.requests().await;
    let post = hits(&requests, STREAM)
        .last()
        .copied()
        .cloned()
        .expect("a stream POST");
    let raw = String::from_utf8_lossy(&post.body).into_owned();
    let generation = serde_json::from_slice::<Value>(&post.body)
        .map(|body| body["request"]["generationConfig"].clone())
        .unwrap_or(Value::Null);
    (post, raw, generation)
}

fn envelope(wire: &str) -> String {
    format!(r#"{{"project":"{PROJECT}","model":"{wire}","request":{{"contents":["#)
}

fn gemini() -> Arc<dyn StreamingProvider> {
    build_provider_for_config(
        PROVIDER,
        &crate::conformance_20_1d4_gemini_cli::auth::config(None),
    )
    .expect("built from the stored login")
}

#[tokio::test]
#[serial]
async fn k4_logical_ids_send_their_no_effort_wire_id_and_thinking() {
    let world = World::signed_in().await;
    mount_stream(&world.server).await;
    let provider = gemini();
    for (model, wire, generation) in [
        (
            "gemini-3.1-pro",
            "gemini-3.1-pro-low",
            json!({"maxOutputTokens": 8192, "thinkingConfig": {"includeThoughts": false, "thinkingLevel": "MINIMAL"}}),
        ),
        (
            "gemini-3.7-flash",
            "gemini-3.7-flash-low",
            json!({"maxOutputTokens": 8192, "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "LOW"}}),
        ),
    ] {
        stream(&provider, model, vec![]).await;
        let (post, raw, sent) = last_post(&world).await;
        assert!(raw.starts_with(&envelope(wire)), "{model}: {raw}");
        assert_eq!(sent, generation, "{model}");
        // The wire identity and the replay stamp keep the logical id (omp `model.id`).
        assert_eq!(header(&post, "user-agent"), gemini_cli_agent(model));
    }

    // Bundled ids are byte-identical to 20-1d4's literals.
    for (model, generation) in [
        ("gemini-2.0-flash", r#"{"maxOutputTokens":8192}"#),
        ("gemini-2.5-flash", r#"{"maxOutputTokens":8192}"#),
        (
            "gemini-2.5-pro",
            r#"{"maxOutputTokens":9216,"thinkingConfig":{"includeThoughts":true,"thinkingBudget":1024}}"#,
        ),
        (
            "gemini-3-flash-preview",
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"MINIMAL"}}"#,
        ),
        (
            "gemini-3-pro-preview",
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}"#,
        ),
        (
            "gemini-3.1-flash-lite-preview",
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"MINIMAL"}}"#,
        ),
        (
            "gemini-3.1-pro-preview",
            r#"{"maxOutputTokens":8192,"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"LOW"}}"#,
        ),
    ] {
        stream(&provider, model, vec![]).await;
        let (_, raw, _) = last_post(&world).await;
        assert!(raw.starts_with(&envelope(model)), "{model}: {raw}");
        assert!(
            raw.ends_with(&format!(r#""generationConfig":{generation}}}}}"#)),
            "{model}: {raw}"
        );
    }
}

#[tokio::test]
#[serial]
async fn k4_a_discovered_derived_pair_sends_itself_without_thinking_after_a_restart() {
    let world = World::signed_in().await;
    mount_quota_path(&world.server, &["gemini-9-pro", "gemini-9-pro-thinking"]).await;
    mount_stream(&world.server).await;
    let cfg = app_config();
    let layer = init_provider_layer(&cfg);
    refresh_target(
        target(&layer, PROVIDER),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await;

    // Control: an adapter that serves no derived pair applies the class rules.
    stream(&gemini(), "gemini-9-pro", vec![]).await;
    let (_, raw, generation) = last_post(&world).await;
    assert!(raw.starts_with(&envelope("gemini-9-pro")), "{raw}");
    assert_eq!(
        generation,
        json!({"maxOutputTokens": 8192, "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "LOW"}})
    );

    // A restart serves the cached roster; the pair routes off to itself.
    let restarted = init_provider_layer(&cfg);
    let provider = restarted
        .router
        .get_provider(PROVIDER)
        .expect("built from the stored login");
    stream(&provider, "gemini-9-pro", vec![]).await;
    let (_, raw, generation) = last_post(&world).await;
    assert!(raw.starts_with(&envelope("gemini-9-pro")), "{raw}");
    assert_eq!(generation, json!({"maxOutputTokens": 8192}));
}

#[tokio::test]
#[serial]
async fn k4_a_partial_roster_routes_to_live_members_and_a_raw_row_to_itself() {
    let world = World::signed_in().await;
    mount_quota_path(
        &world.server,
        &[
            "gemini-pro-agent",
            "gemini-3.7-flash-high",
            "gemini-9-flash",
        ],
    )
    .await;
    mount_stream(&world.server).await;
    let layer = init_provider_layer(&app_config());
    refresh_target(
        target(&layer, PROVIDER),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await;
    let provider = layer
        .router
        .get_provider(PROVIDER)
        .expect("built from the stored login");
    for (model, wire, generation) in [
        // `gemini-3.1-pro-low` is not listed: omp drops the off route and sends
        // the default member, still with the suppress-when-off thinking.
        (
            "gemini-3.1-pro",
            "gemini-pro-agent",
            json!({"maxOutputTokens": 8192, "thinkingConfig": {"includeThoughts": false, "thinkingLevel": "MINIMAL"}}),
        ),
        // Only `-high` is listed: the mandatory effort clamps to its tier.
        (
            "gemini-3.7-flash",
            "gemini-3.7-flash-high",
            json!({"maxOutputTokens": 8192, "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "HIGH"}}),
        ),
        // A raw row named like a `{rev}` logical id has no members: sent as itself.
        (
            "gemini-9-flash",
            "gemini-9-flash",
            json!({"maxOutputTokens": 8192, "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "MINIMAL"}}),
        ),
    ] {
        stream(&provider, model, vec![]).await;
        let (_, raw, sent) = last_post(&world).await;
        assert!(raw.starts_with(&envelope(wire)), "{model}: {raw}");
        assert_eq!(sent, generation, "{model}");
    }
}

// ── DF-20-1D4A-SCHEMA-DEREF-BYTE-BUDGET ───────────────────────────────────

/// `levels` definitions each referencing the next twice, every one carrying two
/// 512-byte literal leaves: 2^(levels+1) inlined copies without a byte budget.
fn doubling_literals(levels: usize) -> Value {
    let literal = "L".repeat(512);
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
    json!({"type": "object", "properties": {"root": {"$ref": "#/$defs/d0"}}, "$defs": defs})
}

/// `levels` nested `properties` without any definitions.
fn deep_properties(levels: usize) -> Value {
    let mut node = json!({"type": "string"});
    for _ in 0..levels {
        node = json!({"type": "object", "properties": {"a": node}});
    }
    node
}

fn tool(name: &str, input_schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("{name} fixture"),
        input_schema,
        parallel_safe: false,
    }
}

#[tokio::test]
#[serial]
async fn df_tool_schema_inlining_is_byte_and_depth_bounded_and_warns_once() {
    let world = World::signed_in().await;
    mount_stream(&world.server).await;
    let provider = gemini();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    // Thread-scoped: the current-thread runtime encodes on this thread.
    let trace_guard = tracing::subscriber::set_default(subscriber);

    // 8,192 inlined copies of 1 KiB of literals would be ~9 MB; the byte budget
    // stops inlining near 1 MiB and degrades the rest to `{}`.
    stream(
        &provider,
        MODEL,
        vec![tool("doubling", doubling_literals(12))],
    )
    .await;
    let (_, raw, _) = last_post(&world).await;
    assert!(raw.len() < 3 << 20, "{} bytes sent", raw.len());
    assert!(
        raw.contains(&"L".repeat(512)),
        "literals inline within the budget"
    );
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("exceeded a $ref inlining bound").count(),
        1,
        "one warning per walk: {logs}"
    );

    // Without `$defs`, nesting past the depth bound is cut to `{}` all the same.
    stream(&provider, MODEL, vec![tool("deep", deep_properties(100))]).await;
    let (_, raw, _) = last_post(&world).await;
    assert_eq!(raw.matches(r#""properties":"#).count(), 65, "{raw}");
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("exceeded a $ref inlining bound").count(),
        2,
        "{logs}"
    );
    assert!(logs.contains("Depth"), "{logs}");

    // Within both bounds nothing is cut and nothing warns (positive control).
    stream(&provider, MODEL, vec![tool("small", doubling_literals(3))]).await;
    let (_, raw, _) = last_post(&world).await;
    assert!(raw.contains(&"L".repeat(512)));
    assert!(
        !raw.contains(r#""a":{}"#) && !raw.contains(r#""b":{}"#),
        "{raw}"
    );
    stream(&provider, MODEL, vec![tool("shallow", deep_properties(30))]).await;
    let (_, raw, _) = last_post(&world).await;
    assert_eq!(raw.matches(r#""properties":"#).count(), 30);
    drop(trace_guard);
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("exceeded a $ref inlining bound").count(),
        2,
        "{logs}"
    );
}
