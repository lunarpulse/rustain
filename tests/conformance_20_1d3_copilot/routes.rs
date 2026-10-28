//! Copilot inference routing (K06), request headers (K07) and warm/cold native
//! replay (K11) through `build_provider_for_config`, every row from a real
//! `flows::login` device login against the loopback stub (AC7).

use super::common::{
    ENDPOINT_A, PROVIDER, ROOT_A, World, assistant, build, chat_path, complete, header, json_body,
    login, login_with, messages_path, mount_routes, options, probe_reply, reply, requests,
    responses_path, responses_sse, responses_text, text_of, tool_result, user, with_image,
};
use crate::conformance_20_1d1b_codex_replay_discovery::common::{completed, enc, reasoning};
use rustain::domain::models::{Message, ProviderReplayItem, StreamChunk, ToolUseMessage};
use serde_json::{Value, json};
use serial_test::serial;
use wiremock::{MockServer, Request};

const PLACEHOLDER: &str = "[image omitted: model does not support vision]";
const IDENTITY: &str = "You are Claude Code";

// ── helpers ─────────────────────────────────────────────────────────────────

/// A logged-in world with all three routes under `/tenant-a` answering `ok`.
async fn setup(token: &str) -> (MockServer, World) {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, token, ENDPOINT_A).await;
    mount_routes(&server, ROOT_A, "ok").await;
    (server, world)
}

/// The single request that reached `route`.
async fn only(server: &MockServer, route: &str) -> Request {
    let mut all = requests(server, "POST", route).await;
    assert_eq!(all.len(), 1, "exactly one POST {route}");
    all.remove(0)
}

/// Every JSON object nested anywhere in `value`.
fn objects(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            out.push(value.clone());
            for child in map.values() {
                objects(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                objects(child, out);
            }
        }
        _ => {}
    }
}
fn nested(value: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    objects(value, &mut out);
    out
}

fn raw(request: &Request) -> String {
    String::from_utf8(request.body.clone()).unwrap()
}

fn input_has_reasoning(body: &Value) -> bool {
    body["input"]
        .as_array()
        .expect("input array")
        .iter()
        .any(|item| item["type"] == "reasoning")
}

fn replays(chunks: &[StreamChunk]) -> Vec<ProviderReplayItem> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ProviderReplay { item } => Some(item.clone()),
            _ => None,
        })
        .collect()
}

// ── K06: route matrix and bodies ────────────────────────────────────────────

/// K06 (M06): each model id reaches its route under the probed root and
/// answers `ok`; `grok-code-fast-1` goes to Chat Completions.
#[tokio::test]
#[serial]
async fn route_matrix_by_model_id() {
    let (server, _world) = setup("gho_routes_matrix").await;
    let cases = [
        ("claude-sonnet-4.5", messages_path(ROOT_A)),
        ("gpt-5.5", responses_path(ROOT_A)),
        ("grok-4.6", responses_path(ROOT_A)),
        ("mai-code-1.1-flash", responses_path(ROOT_A)),
        ("oswe-test", responses_path(ROOT_A)),
        ("grok-code-fast-1", chat_path(ROOT_A)),
        ("gpt-4o", chat_path(ROOT_A)),
        ("gemini-3.1-pro-preview", chat_path(ROOT_A)),
        ("claude-3-5-sonnet", chat_path(ROOT_A)),
    ];
    for (model, route) in cases {
        let before = requests(&server, "POST", &route).await.len();
        let provider = build(model);
        let chunks = complete(&provider, vec![user("hi")], options(model, 8192, None))
            .await
            .unwrap_or_else(|error| panic!("{model}: {error:?}"));
        assert_eq!(text_of(&chunks), "ok", "{model} text");
        let after = requests(&server, "POST", &route).await;
        assert_eq!(after.len(), before + 1, "{model} → {route}");
        assert_eq!(
            after.last().unwrap().url.query(),
            None,
            "{model}: no query string"
        );
        let body = json_body(after.last().unwrap());
        assert_eq!(body["model"], model, "{model}: wire model");
    }
}

/// K06 (M06): the Messages request carries no Claude Code profile: no
/// `anthropic-beta`, no `x-api-key`, no `?beta=true`, no identity block; the
/// session id header is sent and `max_tokens` is clamped to the row max.
#[tokio::test]
#[serial]
async fn messages_literal_has_no_claude_code_profile() {
    let (server, _world) = setup("gho_routes_messages").await;
    let provider = build("claude-sonnet-4.5");
    complete(
        &provider,
        vec![user("hi")],
        options("claude-sonnet-4.5", 8192, Some("sess-msg")),
    )
    .await
    .unwrap();
    let request = only(&server, &messages_path(ROOT_A)).await;
    assert_eq!(request.url.query(), None);
    assert_eq!(header(&request, "anthropic-beta"), None);
    assert_eq!(header(&request, "x-api-key"), None);
    assert_eq!(header(&request, "x-stainless-timeout"), None);
    assert_eq!(
        header(&request, "authorization").as_deref(),
        Some("Bearer gho_routes_messages")
    );
    assert_eq!(
        header(&request, "anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(
        header(&request, "anthropic-dangerous-direct-browser-access").as_deref(),
        Some("true")
    );
    assert_eq!(
        header(&request, "x-claude-code-session-id").as_deref(),
        Some("sess-msg")
    );
    assert!(!raw(&request).contains(IDENTITY), "no identity block");
    let body = json_body(&request);
    assert_eq!(body["max_tokens"], 8192);

    complete(
        &provider,
        vec![user("hi")],
        options("claude-sonnet-4.5", 40000, Some("sess-msg")),
    )
    .await
    .unwrap();
    let all = requests(&server, "POST", &messages_path(ROOT_A)).await;
    assert_eq!(json_body(all.last().unwrap())["max_tokens"], 32000);
}

/// K06 (M06): Responses body for a reasoning row: developer system item, no
/// `instructions`, `store:false`, `max_output_tokens`, temperature dropped,
/// `include` last, `input_image` with `detail:"auto"`.
#[tokio::test]
#[serial]
async fn responses_body_for_reasoning_row() {
    let (server, _world) = setup("gho_routes_responses").await;
    let provider = build("gpt-5.5");
    let mut opts = options("gpt-5.5", 8192, None);
    opts.system_prompt = "SYS".into();
    opts.temperature = Some(0.5);
    complete(&provider, vec![with_image(user("look"))], opts)
        .await
        .unwrap();
    let request = only(&server, &responses_path(ROOT_A)).await;
    let body = json_body(&request);
    assert_eq!(
        body["input"][0],
        json!({"role":"developer","content":"SYS"})
    );
    assert!(body.get("instructions").is_none(), "no instructions");
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 8192);
    assert!(body.get("temperature").is_none(), "temperature dropped");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    let text = raw(&request);
    let include = text.find("\"include\"").expect("include key");
    let max = text.find("\"max_output_tokens\"").expect("max key");
    assert!(include > max, "include after max_output_tokens");
    if let Some(tools) = text.find("\"tools\"") {
        assert!(include > tools, "include after tools");
    }
    // `include` is the final top-level key: nothing but its value follows it.
    let tail = &text[include..];
    let value_end = tail.find(']').expect("include array end");
    assert_eq!(tail[value_end + 1..].trim(), "}", "include is the last key");
    let image = nested(&body)
        .into_iter()
        .find(|item| item["type"] == "input_image")
        .expect("input_image item");
    assert_eq!(image["detail"], "auto");
    assert_eq!(
        header(&request, "copilot-vision-request").as_deref(),
        Some("true")
    );
}

/// K06 (M06): a non-vision Responses row replaces the image with the
/// placeholder text.
#[tokio::test]
#[serial]
async fn responses_non_vision_placeholder() {
    let (server, _world) = setup("gho_routes_novision").await;
    let provider = build("mai-code-1-flash-picker");
    complete(
        &provider,
        vec![with_image(user("look"))],
        options("mai-code-1-flash-picker", 8192, None),
    )
    .await
    .unwrap();
    let request = only(&server, &responses_path(ROOT_A)).await;
    let text = raw(&request);
    assert!(text.contains(PLACEHOLDER), "placeholder text");
    assert!(!text.contains("input_image"), "no input_image");
}

/// K06 (M06): Chat body for `grok-code-fast-1` (system message, `store:false`,
/// `max_completion_tokens`, usage in stream).
#[tokio::test]
#[serial]
async fn chat_body_for_grok_code() {
    let (server, _world) = setup("gho_routes_grokcode").await;
    let provider = build("grok-code-fast-1");
    let mut opts = options("grok-code-fast-1", 8192, None);
    opts.system_prompt = "SYS".into();
    complete(&provider, vec![user("hi")], opts).await.unwrap();
    let request = only(&server, &chat_path(ROOT_A)).await;
    let body = json_body(&request);
    assert_eq!(body["messages"][0]["role"], "system");
    assert!(raw(&request).contains("SYS"));
    assert_eq!(body["store"], false);
    assert_eq!(body["max_completion_tokens"], 8192);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(
        header(&request, "x-stainless-timeout").as_deref(),
        Some("300")
    );
}

/// K06 (M06): `gpt-4o` clamps to its 4096 max and sends image data URLs.
#[tokio::test]
#[serial]
async fn chat_body_for_gpt_4o_vision() {
    let (server, _world) = setup("gho_routes_gpt4o").await;
    let provider = build("gpt-4o");
    complete(
        &provider,
        vec![with_image(user("look"))],
        options("gpt-4o", 8192, None),
    )
    .await
    .unwrap();
    let request = only(&server, &chat_path(ROOT_A)).await;
    let body = json_body(&request);
    assert_eq!(body["max_completion_tokens"], 4096);
    let url = nested(&body)
        .into_iter()
        .find_map(|item| item["image_url"]["url"].as_str().map(str::to_owned))
        .expect("image_url part");
    assert_eq!(url, "data:image/png;base64,AA==");
}

/// K06 (M06): `gemini-3.1-pro-preview` sends no `store` key.
#[tokio::test]
#[serial]
async fn chat_body_for_gemini_has_no_store() {
    let (server, _world) = setup("gho_routes_gemini").await;
    let provider = build("gemini-3.1-pro-preview");
    complete(
        &provider,
        vec![user("hi")],
        options("gemini-3.1-pro-preview", 8192, None),
    )
    .await
    .unwrap();
    let body = json_body(&only(&server, &chat_path(ROOT_A)).await);
    assert!(body.get("store").is_none(), "no store key");
}

// ── K07: headers ────────────────────────────────────────────────────────────

fn assert_constant_headers(request: &Request) {
    for (name, value) in [
        ("x-github-api-version", "2026-08-01"),
        ("copilot-harness-id", "copilot-sdk"),
        ("openai-intent", "conversation-agent"),
        ("user-agent", "copilot/1.0.82"),
        ("editor-version", "copilot/1.0.82"),
        ("accept", "text/event-stream"),
    ] {
        assert_eq!(header(request, name).as_deref(), Some(value), "{name}");
    }
}

/// K07 (M07): initiator headers follow the tail: user tail → user; assistant
/// tail and trailing tool-result user message → agent; vision header only
/// with an image.
#[tokio::test]
#[serial]
async fn initiator_follows_conversation_tail() {
    let (server, _world) = setup("gho_routes_initiator").await;
    let provider = build("gpt-4o");
    let mut caller = assistant("");
    caller.tool_uses.push(ToolUseMessage {
        id: "call_1".into(),
        name: "read".into(),
        input: json!({}),
    });
    let histories: Vec<(Vec<Message>, &str, &str)> = vec![
        (vec![user("hi")], "user", "conversation-user"),
        (
            vec![user("hi"), assistant("prefill")],
            "agent",
            "conversation-agent",
        ),
        (
            vec![user("hi"), caller, tool_result("call_1", "done")],
            "agent",
            "conversation-agent",
        ),
    ];
    for (history, initiator, interaction) in histories {
        complete(&provider, history, options("gpt-4o", 1024, None))
            .await
            .unwrap();
        let all = requests(&server, "POST", &chat_path(ROOT_A)).await;
        let request = all.last().unwrap();
        assert_eq!(header(request, "x-initiator").as_deref(), Some(initiator));
        assert_eq!(
            header(request, "x-interaction-type").as_deref(),
            Some(interaction)
        );
        assert_eq!(header(request, "copilot-vision-request"), None);
        assert_constant_headers(request);
    }
    complete(
        &provider,
        vec![with_image(user("look"))],
        options("gpt-4o", 1024, None),
    )
    .await
    .unwrap();
    let all = requests(&server, "POST", &chat_path(ROOT_A)).await;
    assert_eq!(
        header(all.last().unwrap(), "copilot-vision-request").as_deref(),
        Some("true")
    );
}

/// K07: constant headers on every route; `x-stainless-timeout` only on
/// Responses and Chat; public login defaults to `copilot-chat`.
#[tokio::test]
#[serial]
async fn constant_headers_on_every_route() {
    let (server, _world) = setup("gho_routes_constant").await;
    for model in ["claude-sonnet-4.5", "gpt-5.5", "gpt-4o"] {
        let provider = build(model);
        complete(&provider, vec![user("hi")], options(model, 1024, None))
            .await
            .unwrap();
    }
    let messages = only(&server, &messages_path(ROOT_A)).await;
    let responses = only(&server, &responses_path(ROOT_A)).await;
    let chat = only(&server, &chat_path(ROOT_A)).await;
    for request in [&messages, &responses, &chat] {
        assert_constant_headers(request);
        assert_eq!(
            header(request, "copilot-integration-id").as_deref(),
            Some("copilot-chat")
        );
        assert_eq!(
            header(request, "authorization").as_deref(),
            Some("Bearer gho_routes_constant")
        );
    }
    assert_eq!(header(&messages, "x-stainless-timeout"), None);
    assert_eq!(
        header(&responses, "x-stainless-timeout").as_deref(),
        Some("300")
    );
    assert_eq!(header(&chat, "x-stainless-timeout").as_deref(), Some("300"));
}

/// K07: an Enterprise login defaults to `copilot-developer-cli`.
#[tokio::test]
#[serial]
async fn enterprise_login_defaults_to_cli_identity() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login_with(
        &server,
        "company.ghe.com",
        "gho_routes_enterprise",
        probe_reply(ENDPOINT_A),
    )
    .await;
    mount_routes(&server, ROOT_A, "ok").await;
    let provider = build("gpt-4o");
    complete(&provider, vec![user("hi")], options("gpt-4o", 1024, None))
        .await
        .unwrap();
    let request = only(&server, &chat_path(ROOT_A)).await;
    assert_eq!(
        header(&request, "copilot-integration-id").as_deref(),
        Some("copilot-developer-cli")
    );
}

/// K07: `COPILOT_INTEGRATION_ID` pins the identity.
#[tokio::test]
#[serial]
async fn integration_id_env_pin_wins() {
    let (server, _world) = setup("gho_routes_pin").await;
    // SAFETY: the serial lock is held; `World` restores the variable on drop.
    unsafe { std::env::set_var("COPILOT_INTEGRATION_ID", "custom-id") };
    let provider = build("gpt-4o");
    complete(&provider, vec![user("hi")], options("gpt-4o", 1024, None))
        .await
        .unwrap();
    let request = only(&server, &chat_path(ROOT_A)).await;
    assert_eq!(
        header(&request, "copilot-integration-id").as_deref(),
        Some("custom-id")
    );
}

// ── K11: warm/cold native replay ────────────────────────────────────────────

async fn mount_reasoning(server: &MockServer, marker: &str) {
    reply(
        server,
        "POST",
        &responses_path(ROOT_A),
        responses_sse(&[reasoning(marker), responses_text("ok"), completed()]),
    )
    .await;
}

fn turn_two(item: ProviderReplayItem) -> Vec<Message> {
    let mut answer = assistant("ok");
    answer.provider_replay = vec![item];
    vec![user("first"), answer, user("second")]
}

/// K11 (M11): replay is sent only once a response in the session completed
/// in this adapter instance; a new provider (new process) starts cold.
#[tokio::test]
#[serial]
async fn replay_warm_in_session_cold_in_new_instance() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "gho_routes_warm", ENDPOINT_A).await;
    mount_reasoning(&server, "warm").await;
    let route = responses_path(ROOT_A);

    let provider = build("gpt-5.5");
    let chunks = complete(
        &provider,
        vec![user("first")],
        options("gpt-5.5", 8192, Some("S")),
    )
    .await
    .unwrap();
    let first = json_body(&requests(&server, "POST", &route).await[0]);
    assert!(!input_has_reasoning(&first), "turn 1 has no replay");
    assert_eq!(first["prompt_cache_key"], "S");
    let items = replays(&chunks);
    assert_eq!(items.len(), 1, "one captured replay item");
    let item = items[0].clone();
    assert_eq!(item.provider, PROVIDER);
    assert_eq!(item.model, "gpt-5.5");

    complete(
        &provider,
        turn_two(item.clone()),
        options("gpt-5.5", 8192, Some("S")),
    )
    .await
    .unwrap();
    let second_request = requests(&server, "POST", &route).await[1].clone();
    let second = json_body(&second_request);
    assert!(input_has_reasoning(&second), "warm turn replays reasoning");
    assert!(
        raw(&second_request).contains(&enc("warm")),
        "encrypted payload"
    );

    let fresh = build("gpt-5.5");
    complete(&fresh, turn_two(item), options("gpt-5.5", 8192, Some("S")))
        .await
        .unwrap();
    let third_request = requests(&server, "POST", &route).await[2].clone();
    assert!(
        !input_has_reasoning(&json_body(&third_request)),
        "new instance is cold"
    );
    assert!(!raw(&third_request).contains(&enc("warm")));
}

/// K11 (M11): without a session id the adapter is always cold.
#[tokio::test]
#[serial]
async fn replay_without_session_is_always_cold() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "gho_routes_cold", ENDPOINT_A).await;
    mount_reasoning(&server, "cold").await;
    let route = responses_path(ROOT_A);

    let provider = build("gpt-5.5");
    let chunks = complete(
        &provider,
        vec![user("first")],
        options("gpt-5.5", 8192, None),
    )
    .await
    .unwrap();
    let items = replays(&chunks);
    assert_eq!(items.len(), 1);
    complete(
        &provider,
        turn_two(items[0].clone()),
        options("gpt-5.5", 8192, None),
    )
    .await
    .unwrap();
    let all = requests(&server, "POST", &route).await;
    assert_eq!(all.len(), 2);
    for request in &all {
        assert!(!input_has_reasoning(&json_body(request)));
        assert!(!raw(request).contains(&enc("cold")));
    }
}
