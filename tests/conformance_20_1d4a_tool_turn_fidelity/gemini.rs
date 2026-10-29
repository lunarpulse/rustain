//! The AC2 Gemini tool loop on every persister: K1 (TUI reload), K2 (`ask`
//! resume), K3 (daemon restart), K4 (image wire per model class), K5 (privacy
//! of every outward path, protocol branch), K9 (cross-rail resume) and the ACP
//! round trip. The oracle is always the last in-turn POST.
use super::harness::{
    Daemon, ECHO_TOOL, IMAGE, IMAGE_TOOL, Tools, ask, assert_prefix, attach, daemon_turn, history,
    tui_turn, until_end_turn,
};
use crate::conformance_20_1d1b_codex_replay_discovery::common::{opts, sessions_contain, storage};
use crate::conformance_20_1d4_gemini_cli::auth::{MODEL, PROVIDER, build, config};
use crate::conformance_20_1d4_gemini_cli::replay::{
    Capture, World, body, contents, frame, leaks, render, signatures, sse,
};
use rustain::adapters::cli::ask::OutputFormat;
use rustain::adapters::daemon::protocol::{ConnectionTier, DaemonFrame, PROTOCOL_VERSION};
use rustain::domain::models::{AppConfig, ContentBlockType, Conversation, MessageRole};
use rustain::domain::ports::StoragePort;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::Path;
use std::process::ExitCode;
use wiremock::ResponseTemplate;

/// Fixture signatures; every one starts with the Base64 of `SIG`.
const SIG: [&str; 5] = ["U0lHMA==", "U0lHMQ==", "U0lHMg==", "U0lHMw==", "U0lHNA=="];
const PRO_25: &str = "gemini-2.5-pro";

fn app_config(model: &str) -> AppConfig {
    let mut cfg = AppConfig {
        model: model.into(),
        ..Default::default()
    };
    cfg.provider.insert(PROVIDER.into(), config(None));
    cfg
}

// ── the AC2 turn ────────────────────────────────────────────────────────────

/// Response 1: a signed thought, signed text, signed call A (the image) and
/// unsigned call B (the echo, which finishes first).
fn response_1() -> ResponseTemplate {
    sse(&[
        frame(
            json!([{"thought": true, "text": "T", "thoughtSignature": SIG[0]}]),
            None,
        ),
        frame(json!([{"text": "V"}]), None),
        frame(json!([{"text": "", "thoughtSignature": SIG[1]}]), None),
        frame(
            json!([
                {"functionCall": {"name": IMAGE_TOOL, "args": {"label": "a"}}, "thoughtSignature": SIG[2]},
                {"functionCall": {"name": ECHO_TOOL, "args": {"text": "b"}}},
            ]),
            Some("STOP"),
        ),
    ])
}

/// Response 2: signed call C.
fn response_2() -> ResponseTemplate {
    sse(&[frame(
        json!([{"functionCall": {"name": ECHO_TOOL, "args": {"text": "c"}}, "thoughtSignature": SIG[3]}]),
        Some("STOP"),
    )])
}

/// Response 3: the signed final text.
fn response_3() -> ResponseTemplate {
    sse(&[frame(
        json!([{"text": "done", "thoughtSignature": SIG[4]}]),
        Some("STOP"),
    )])
}

fn answer(text: &str) -> ResponseTemplate {
    sse(&[frame(json!([{"text": text}]), Some("STOP"))])
}

/// The three requests of one AC2 turn, then plain answers.
fn script(answers: &[&str]) -> Vec<ResponseTemplate> {
    let mut script = vec![response_1(), response_2(), response_3()];
    script.extend(answers.iter().map(|text| answer(text)));
    script
}

/// Response 3's model turn, as G maps the stub's scripted parts.
fn response_3_model() -> Value {
    json!({"role": "model", "parts": [{"text": "done", "thoughtSignature": SIG[4]}]})
}

fn user_content(text: &str) -> Value {
    json!({"role": "user", "parts": [{"text": text}]})
}

/// The reloaded side of the oracle: POST `at` continues the in-turn POST
/// `at - 1` with response 3's model turn and the new prompt.
fn assert_reload(posts: &[String], at: usize, prompt: &str) {
    let rest = assert_prefix(&posts[at - 1], &posts[at], "contents");
    assert_eq!(rest, vec![response_3_model(), user_content(prompt)]);
}

/// The in-turn POST 3 itself: per-response model turns with every signature on
/// its own part, call-ordered results with the image nested in call A's
/// response (a `multimodalFunctionResponse` model).
fn assert_in_turn(post_3: &str) {
    let contents = contents(post_3);
    let roles: Vec<_> = contents.iter().map(|c| c["role"].clone()).collect();
    assert_eq!(
        roles,
        ["user", "model", "user", "model", "user"],
        "{post_3}"
    );
    assert_eq!(signatures(&contents[1]), SIG[..3].to_vec(), "{post_3}");
    assert_eq!(
        contents[2],
        json!({"role": "user", "parts": [
            {"functionResponse": {"name": IMAGE_TOOL, "response": {"output": "image a\n[image: image/png]"},
                "parts": [{"inlineData": {"mimeType": "image/png", "data": IMAGE}}]}},
            {"functionResponse": {"name": ECHO_TOOL, "response": {"output": "echo b"}}},
        ]}),
        "{post_3}"
    );
    assert_eq!(signatures(&contents[3]), vec![SIG[3].to_owned()]);
}

// ── K1: TUI reload ──────────────────────────────────────────────────────────

/// Turn 1 through the TUI composition, the session save, optionally a reload
/// from the store, then turn 2. Returns every POST and the final conversation.
async fn tui_run(reload: bool) -> (Vec<String>, Conversation) {
    let world = World::start().await;
    world.script(script(&["after reload"])).await;
    let ws = world.workspace();
    let cfg = app_config(MODEL);
    let store = storage(&ws);
    let mut conversation = Conversation {
        id: "k1-tui".into(),
        ..Default::default()
    };
    tui_turn(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        &mut conversation,
        "first",
    )
    .await;
    store.save_conversation(&conversation).await.unwrap();
    if reload {
        conversation = store.load_conversation("k1-tui").await.unwrap().unwrap();
    }
    tui_turn(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        &mut conversation,
        "next",
    )
    .await;
    store.save_conversation(&conversation).await.unwrap();
    let posts = world.posts().await;
    assert_eq!(posts.len(), 4, "three in-turn requests, then the next turn");
    (posts, conversation)
}

/// K1 (AC2, TUI): M04 makes the reloaded history one merged message.
#[tokio::test]
#[serial]
async fn k1_tui_reload_reproduces_the_in_turn_history() {
    let (posts, conversation) = tui_run(true).await;
    assert_in_turn(&posts[2]);
    assert_reload(&posts, 3, "next");
    // TUI rows are unchanged: one row per turn, joined by id, `tc_…` calls.
    let assistant: Vec<_> = conversation
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .collect();
    assert_eq!(assistant.len(), conversation.turns.len());
    for (row, turn) in assistant.iter().zip(&conversation.turns) {
        assert_eq!(row.id, turn.id.0);
        assert!(row.tool_calls.iter().all(|call| call.id.starts_with("tc_")));
    }
    assert_eq!(
        assistant[0].content, "VTdone",
        "the display fold is unchanged"
    );
    let screen = render(&conversation);
    // A three-tool turn renders folded: one row, no marker.
    assert!(
        screen.contains("3 tools") && !leaks(&screen) && !screen.contains(IMAGE),
        "{screen}"
    );

    let (control, _) = tui_run(false).await;
    assert_eq!(
        posts[3], control[3],
        "the no-reload control sends the same bytes"
    );
}

// ── K2: `ask` resume ────────────────────────────────────────────────────────

/// K2 (AC2, `ask`): M03 merges the turn's responses, M06 sends the renamed
/// `tc_…` id that 20-1d4's guard then refuses.
#[tokio::test]
#[serial]
async fn k2_ask_resume_reproduces_the_in_turn_history_and_output_is_unchanged() {
    let world = World::start().await;
    world.script(script(&["after resume"])).await;
    let ws = world.workspace();
    let cfg = app_config(MODEL);
    let (code, stdout, stderr) = ask(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        opts("first", OutputFormat::Json, None),
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).unwrap();
    // `ask --output-format json` keeps its DTO bytes: visible text only.
    assert_eq!(doc["response"], "Vdone");
    let names: Vec<_> = doc["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, [IMAGE_TOOL, ECHO_TOOL, ECHO_TOOL]);
    assert_eq!(
        doc["tool_calls"][0]["result"]["content"],
        "image a\n[image: image/png]"
    );
    for out in [&stdout, &stderr] {
        assert!(!leaks(out) && !out.contains(IMAGE), "{out}");
    }
    let session = doc["session_id"].as_str().unwrap().to_owned();

    let (code, stdout, stderr) = ask(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        opts("next", OutputFormat::Text, Some(session)),
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    assert!(stdout.contains("after resume"), "{stdout}");
    assert!(!leaks(&stdout) && !stdout.contains(IMAGE), "{stdout}");
    let posts = world.posts().await;
    assert_eq!(posts.len(), 4);
    assert_in_turn(&posts[2]);
    assert_reload(&posts, 3, "next");
    assert!(sessions_contain(&ws, SIG[2]) && sessions_contain(&ws, IMAGE));
}

// ── K3: daemon restart ──────────────────────────────────────────────────────

const DAEMON: &str = "k3-daemon";

async fn daemon_run(restart: bool) -> Vec<String> {
    let world = World::start().await;
    world.script(script(&["after restart"])).await;
    let ws = world.workspace();
    let cfg = app_config(MODEL);
    let conversation = Conversation {
        id: DAEMON.into(),
        ..Default::default()
    };
    let mut daemon = Daemon::start(build(), &cfg, &ws, conversation, "first.sock");
    let (mut client, _) = attach(&daemon.socket, ConnectionTier::TrustedLocal, false).await;
    daemon_turn(&mut client, "first").await;
    if restart {
        drop(client);
        daemon = daemon
            .restart(build(), &cfg, &ws, DAEMON, "second.sock")
            .await;
        client = attach(&daemon.socket, ConnectionTier::TrustedLocal, false)
            .await
            .0;
    }
    daemon_turn(&mut client, "next").await;
    drop(client);
    daemon.stop().await;
    let posts = world.posts().await;
    assert_eq!(posts.len(), 4);
    posts
}

/// K3 (AC2, daemon): M01 keeps the replay blocks but persists no calls.
#[tokio::test]
#[serial]
async fn k3_daemon_restart_reproduces_the_in_turn_history() {
    let posts = daemon_run(true).await;
    assert_in_turn(&posts[2]);
    assert_reload(&posts, 3, "next");
    assert!(
        !posts[3].contains("[tool use:"),
        "never prose: {}",
        posts[3]
    );
    let control = daemon_run(false).await;
    assert_eq!(
        posts[3], control[3],
        "the no-restart control sends the same bytes"
    );
}

// ── K4: image wire per model class ──────────────────────────────────────────

/// K4 (AC1): `gemini-3.1-pro-preview` nests the image in the call-ordered
/// result turn (M02 RED); `gemini-2.5-pro` sends a separate `Tool result image:`
/// user turn after it. Both through `project_rmcp_result` and the turn loop.
#[tokio::test]
#[serial]
async fn k4_result_images_reach_the_gemini_wire_per_model_class() {
    for model in [MODEL, PRO_25] {
        let world = World::start().await;
        world.script(script(&[])).await;
        let ws = world.workspace();
        let (code, _, stderr) = ask(
            build(),
            Tools::port(true),
            &app_config(model),
            &ws,
            opts("first", OutputFormat::Text, None),
        )
        .await;
        assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
        let posts = world.posts().await;
        assert_eq!(posts.len(), 3);
        let after_results = contents(&posts[1]);
        let image_result =
            json!({"name": IMAGE_TOOL, "response": {"output": "image a\n[image: image/png]"}});
        let echo_result =
            json!({"functionResponse": {"name": ECHO_TOOL, "response": {"output": "echo b"}}});
        let inline = json!({"inlineData": {"mimeType": "image/png", "data": IMAGE}});
        if model == MODEL {
            let mut nested = image_result.clone();
            nested["parts"] = json!([inline]);
            assert_eq!(after_results.len(), 3, "{}", posts[1]);
            assert_eq!(
                after_results[2],
                json!({"role": "user", "parts": [{"functionResponse": nested}, echo_result]})
            );
        } else {
            assert_eq!(after_results.len(), 4, "{}", posts[1]);
            assert_eq!(
                after_results[2],
                json!({"role": "user", "parts": [{"functionResponse": image_result}, echo_result]})
            );
            assert_eq!(
                after_results[3],
                json!({"role": "user", "parts": [{"text": "Tool result image:"}, inline]})
            );
        }
        assert_eq!(body(&posts[1])["model"], model);
    }
}

// ── K5: privacy of every outward path, protocol branch ──────────────────────

fn any_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| k == key || any_key(v, key)),
        Value::Array(items) => items.iter().any(|v| any_key(v, key)),
        _ => false,
    }
}

/// No image bytes, signature, replay item, span or provider call id.
fn clean(frames: &[DaemonFrame]) {
    for frame in frames {
        let encoded = serde_json::to_string(frame).unwrap();
        assert!(!leaks(&encoded) && !encoded.contains(IMAGE), "{encoded}");
        let value: Value = serde_json::from_str(&encoded).unwrap();
        for key in ["providerReplay", "responseSpan", "providerId", "images"] {
            assert!(!any_key(&value, key), "{key}: {encoded}");
        }
    }
}

/// Every tool call of a transcript frame, as JSON.
fn transcript_calls(frame: &DaemonFrame) -> Vec<Value> {
    fn walk(value: &Value, out: &mut Vec<Value>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::Array(calls)) = map.get("toolCalls") {
                    out.extend(calls.iter().cloned());
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&serde_json::to_value(frame).unwrap(), &mut out);
    out
}

/// K5 (AC4, AC5): a client attached during the image turn, a client attached
/// after it (snapshot and `History` page; M05 RED on that snapshot), a
/// Peer-tier client (inputs withheld) and the channel reply carry no marker;
/// the store keeps both; the handshake stays at the landed version.
#[tokio::test]
#[serial]
async fn k5_markers_reach_only_the_session_store_on_every_daemon_path() {
    assert_eq!(PROTOCOL_VERSION, 15, "the default branch: no protocol bump");
    let world = World::start().await;
    let mut attempts = script(&[]);
    attempts.extend(script(&[]));
    world.script(attempts).await;
    let ws = world.workspace();
    let cfg = app_config(MODEL);
    let daemon = Daemon::start(
        build(),
        &cfg,
        &ws,
        Conversation {
            id: "k5-daemon".into(),
            ..Default::default()
        },
        "k5.sock",
    );
    let (mut writer, ack) = attach(&daemon.socket, ConnectionTier::TrustedLocal, false).await;
    let (mut live, live_ack) = attach(&daemon.socket, ConnectionTier::TrustedLocal, true).await;
    clean(&[ack, live_ack]);
    clean(&daemon_turn(&mut writer, "first").await);
    let live_frames = until_end_turn(&mut live).await;
    clean(&live_frames);
    assert!(
        live_frames
            .iter()
            .any(|f| serde_json::to_string(f).unwrap().contains("image a")),
        "the live client saw the image result's text"
    );

    let (mut late, snapshot) = attach(&daemon.socket, ConnectionTier::TrustedLocal, true).await;
    clean(std::slice::from_ref(&snapshot));
    let calls = transcript_calls(&snapshot);
    assert_eq!(calls.len(), 3, "structured calls, not prose: {calls:?}");
    assert!(calls.iter().all(|call| !call["input"].is_null()));
    clean(&history(&mut late).await);

    let (_peer, peer_snapshot) = attach(&daemon.socket, ConnectionTier::Peer, true).await;
    clean(std::slice::from_ref(&peer_snapshot));
    let calls = transcript_calls(&peer_snapshot);
    assert_eq!(calls.len(), 3);
    assert!(
        calls.iter().all(|call| call["input"].is_null()),
        "Peer tier: persisted inputs withheld: {calls:?}"
    );

    // The channel reply of an image turn: its first response's prose fold.
    let reply = daemon.channel_turn("via channel").await;
    assert!(
        reply.starts_with("V\n[tool use: fixture_image (id: "),
        "{reply}"
    );
    assert!(!leaks(&reply) && !reply.contains(IMAGE), "{reply}");
    drop((writer, live, late));
    daemon.stop().await;
    assert!(sessions_contain(&ws, SIG[2]) && sessions_contain(&ws, IMAGE));
}

/// AC4: `ask` text/JSON/stream-JSON, ACP updates, the TUI buffer and `tracing`
/// never carry the image bytes or a signature; the stores keep both.
#[tokio::test]
#[serial]
async fn k5_ask_acp_tui_and_tracing_carry_no_marker() {
    let world = World::start().await;
    let mut attempts = Vec::new();
    for _ in 0..4 {
        attempts.extend(script(&[]));
    }
    world.script(attempts).await;
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let trace_guard = tracing::subscriber::set_default(subscriber);
    let ws = world.workspace();
    let cfg = app_config(MODEL);
    for format in [
        OutputFormat::Text,
        OutputFormat::Json,
        OutputFormat::StreamJson,
    ] {
        let (code, stdout, stderr) = ask(
            build(),
            Tools::port(true),
            &cfg,
            &ws,
            opts("first", format, None),
        )
        .await;
        assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
        assert!(stdout.contains("done"), "{stdout}");
        for out in [&stdout, &stderr] {
            assert!(!leaks(out) && !out.contains(IMAGE), "{out}");
        }
    }
    let mut conversation = Conversation {
        id: "k5-tui".into(),
        ..Default::default()
    };
    let chunks = tui_turn(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        &mut conversation,
        "first",
    )
    .await;
    for chunk in &chunks {
        if let Some(update) = rustain::adapters::acp::translate::stream_chunk_to_session_update(
            chunk,
            Path::new("/w"),
        ) {
            let update = format!("{update:?}");
            assert!(!leaks(&update) && !update.contains(IMAGE), "{update}");
        }
        tracing::info!(chunk = ?chunk, "chunk observed");
    }
    let screen = render(&conversation);
    assert!(
        screen.contains("3 tools") && !leaks(&screen) && !screen.contains(IMAGE),
        "{screen}"
    );
    let saved = serde_json::to_string(&conversation).unwrap();
    assert!(saved.contains(IMAGE) && saved.contains(SIG[2]));
    drop(trace_guard);
    let traced = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(traced.contains("chunk observed"), "capture is armed");
    assert!(traced.contains("bytes: 24"), "the redacting Debug ran");
    assert!(!leaks(&traced) && !traced.contains(IMAGE), "{traced}");
    assert!(sessions_contain(&ws, IMAGE) && sessions_contain(&ws, SIG[2]));
}

// ── legacy: a pre-story TUI session with turns ──────────────────────────────

/// Remove every key this story added, recursively: the file a pre-story binary
/// wrote for the same turn.
fn strip_story_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for key in ["responses", "providerId", "images"] {
                map.remove(key);
            }
            if let Some(Value::Array(blocks)) = map.get_mut("contentBlocks") {
                blocks.retain(|block| block.get("responseSpan").is_none());
            }
            map.values_mut().for_each(strip_story_keys);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_story_keys),
        _ => {}
    }
}

/// A pre-story TUI session (turns without response marks) loads and rebuilds
/// as before: one merged message per turn, `tc_…` ids, and the guard's
/// fallback (nothing signed is sent misplaced).
#[tokio::test]
#[serial]
async fn plain_pre_story_tui_session_rebuilds_as_before() {
    let world = World::start().await;
    world.script(script(&["after reload"])).await;
    let ws = world.workspace();
    let cfg = app_config(MODEL);
    let mut conversation = Conversation {
        id: "legacy-tui".into(),
        ..Default::default()
    };
    tui_turn(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        &mut conversation,
        "first",
    )
    .await;
    let mut file = serde_json::to_value(
        rustain::domain::models::conversation::PersistedConversation::from_conversation(
            &conversation,
        ),
    )
    .unwrap();
    strip_story_keys(&mut file);
    let mut legacy = serde_json::from_value::<
        rustain::domain::models::conversation::PersistedConversation,
    >(file)
    .unwrap()
    .to_conversation();
    assert!(legacy.turns.iter().all(|turn| turn.responses.is_empty()));
    for _ in 0..2 {
        let api = rustain::domain::services::message_builder::build_api_messages(&legacy);
        let roles: Vec<_> = api.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [MessageRole::User, MessageRole::Assistant, MessageRole::User]
        );
        assert_eq!(api[1].content, "VTdone");
        assert_eq!(api[1].tool_uses.len(), 3);
        assert!(api[1].tool_uses.iter().all(|u| u.id.starts_with("tc_")));
        assert_eq!(
            api[1].provider_replay.len(),
            3,
            "every item rides the one message"
        );
        legacy.rebuild_messages_mirror();
    }
    tui_turn(build(), Tools::port(true), &cfg, &ws, &mut legacy, "next").await;
    let posts = world.posts().await;
    assert_eq!(posts.len(), 4);
    // Review RF-07: the guard refuses the merged row, which goes out unsigned
    // with only its first call's validator sentinel and all three calls, as
    // the 20-1d4 baseline sent it (an empty signature list must not pass).
    assert_eq!(
        signatures(&body(&posts[3])),
        vec!["skip_thought_signature_validator".to_owned()],
        "the guard refuses the merged row: {}",
        posts[3]
    );
    assert_eq!(
        posts[3].matches("\"functionCall\"").count(),
        3,
        "{}",
        posts[3]
    );
}

// ── K9: cross-rail resume keeps every row ───────────────────────────────────

/// K9 (AC7): `ask`, `--session <id> ask`, then a TUI load of the same session
/// and two TUI turns. M07 (unbacked rows migrated only into an empty `turns`)
/// drops the process-2 row: request A loses its history and the file loses it.
#[tokio::test]
#[serial]
async fn k9_cross_rail_resume_keeps_the_process_2_row() {
    let world = World::start().await;
    let mut attempts = script(&[]);
    attempts.extend(script(&["answer A", "answer B"]));
    world.script(attempts).await;
    let ws = world.workspace();
    let cfg = app_config(MODEL);
    let (code, stdout, stderr) = ask(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        opts("one", OutputFormat::Json, None),
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    let session = serde_json::from_str::<Value>(&stdout).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, _, stderr) = ask(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        opts("two", OutputFormat::Json, Some(session.clone())),
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");

    let store = storage(&ws);
    let mut conversation = store.load_conversation(&session).await.unwrap().unwrap();
    let process_2 = conversation
        .messages
        .iter()
        .rfind(|m| m.role == MessageRole::Assistant)
        .unwrap()
        .id
        .clone();
    assert!(
        conversation.turns.iter().any(|turn| turn.id.0 == process_2),
        "the process-2 answer is a turn-backed row"
    );
    tui_turn(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        &mut conversation,
        "A",
    )
    .await;
    store.save_conversation(&conversation).await.unwrap();
    tui_turn(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        &mut conversation,
        "B",
    )
    .await;
    store.save_conversation(&conversation).await.unwrap();

    let posts = world.posts().await;
    assert_eq!(posts.len(), 8);
    // Request A: process 2's last in-turn request, its final model turn, "A".
    assert_reload(&posts, 6, "A");
    // Request B: request A, then answer A, then "B".
    let rest = assert_prefix(&posts[6], &posts[7], "contents");
    assert_eq!(
        rest,
        vec![
            json!({"role": "model", "parts": [{"text": "answer A"}]}),
            user_content("B")
        ]
    );
    let saved = store.load_conversation(&session).await.unwrap().unwrap();
    assert!(
        saved.messages.iter().any(|m| m.id == process_2),
        "the saved session still holds the process-2 row"
    );
}

// ── ACP: a two-response tool turn, reloaded ─────────────────────────────────

/// ACP persists one row per response; its file, resumed by `ask`, rebuilds the
/// in-turn history.
#[tokio::test]
#[serial]
async fn plain_acp_tool_turn_reloads_its_in_turn_history() {
    let world = World::start().await;
    world.script(script(&["after acp"])).await;
    let ws = world.workspace();
    let cfg = app_config(MODEL);
    let (_, lines) =
        super::providers::acp_notice_turns(build(), &cfg, &ws, None, &["first"], false).await;
    // Review RF-08 (AC4): the ACP agent's own updates carry no signature and no
    // image bytes, while its store keeps both.
    assert!(!lines.is_empty());
    for line in &lines {
        let line = line.to_string();
        assert!(!leaks(&line) && !line.contains(IMAGE), "{line}");
    }
    assert!(sessions_contain(&ws, IMAGE) && sessions_contain(&ws, SIG[2]));
    let store = storage(&ws);
    let summaries = store.list_conversations().await.unwrap();
    assert_eq!(summaries.len(), 1);
    let stored = store
        .load_conversation(&summaries[0].id)
        .await
        .unwrap()
        .unwrap();
    let spans = stored
        .messages
        .iter()
        .flat_map(|m| m.content_blocks.iter())
        .filter(|b| matches!(b, ContentBlockType::ResponseSpan(_)))
        .count();
    assert_eq!(spans, 3, "one span per response row");
    let (code, _, stderr) = ask(
        build(),
        Tools::port(true),
        &cfg,
        &ws,
        opts("next", OutputFormat::Text, Some(summaries[0].id.clone())),
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    let posts = world.posts().await;
    assert_eq!(posts.len(), 4);
    assert_in_turn(&posts[2]);
    assert_reload(&posts, 3, "next");
}

/// Signed ACP history must survive both live notices and the real load/mirror
/// path. The last tool-loop POST is the oracle, not a constructed message.
#[tokio::test]
#[serial]
async fn acp_notice_signed_tool_turn_preserves_wire_history() {
    use super::providers::{FLIGHT_NOTICE, QUEUED_NOTICE, acp_notice_turns};

    const AUTHORED: &str = "Warning: provider-authored prose";
    async fn run(notices: bool) -> Vec<Value> {
        let world = World::start().await;
        world
            .script(vec![
                response_1(),
                response_2(),
                sse(&[frame(
                    json!([{"text": AUTHORED, "thoughtSignature": SIG[4]}]),
                    Some("STOP"),
                )]),
                answer("same answer"),
                answer("loaded answer"),
                answer("rebuilt answer"),
            ])
            .await;
        let ws = world.workspace();
        let cfg = app_config(MODEL);
        let (session, frames) =
            acp_notice_turns(build(), &cfg, &ws, None, &["first", "same"], notices).await;
        let texts: Vec<_> = frames
            .iter()
            .filter_map(|v| v["params"]["update"]["content"]["text"].as_str())
            .collect();
        assert!(texts.contains(&AUTHORED));
        for marker in [QUEUED_NOTICE, FLIGHT_NOTICE] {
            let expected = format!("Warning: {marker}");
            assert_eq!(
                texts.iter().filter(|text| **text == expected).count(),
                usize::from(notices)
            );
        }

        let (_, loaded_frames) =
            acp_notice_turns(build(), &cfg, &ws, Some(&session), &["loaded"], false).await;
        if notices {
            for marker in [QUEUED_NOTICE, FLIGHT_NOTICE] {
                let expected = format!("Warning: {marker}");
                assert_eq!(
                    loaded_frames
                        .iter()
                        .filter(|v| v["params"]["update"]["content"]["text"] == expected)
                        .count(),
                    1
                );
            }
        }
        let store = storage(&ws);
        let summaries = store.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 1);
        let id = &summaries[0].id;
        let before = store.load_conversation(id).await.unwrap().unwrap();
        let calls = |conversation: &Conversation| {
            conversation
                .messages
                .iter()
                .flat_map(|row| &row.tool_calls)
                .map(|call| {
                    (
                        call.wire_id().to_owned(),
                        call.name.clone(),
                        call.result.as_ref().map(|result| result.content.clone()),
                    )
                })
                .collect::<Vec<_>>()
        };
        let original_calls = calls(&before);
        assert_eq!(original_calls.len(), 3);
        assert!(
            original_calls
                .iter()
                .all(|(id, _, result)| !id.is_empty() && result.is_some())
        );
        assert_eq!(
            original_calls
                .iter()
                .map(|(_, name, _)| name.as_str())
                .collect::<Vec<_>>(),
            [IMAGE_TOOL, ECHO_TOOL, ECHO_TOOL]
        );
        assert!(before.messages.iter().flat_map(|row| &row.content_blocks).any(|block| matches!(block, ContentBlockType::ResponseSpan(span) if span.reasoning.as_deref() == Some("T"))));
        for _ in 0..2 {
            let mut conversation = store.load_conversation(id).await.unwrap().unwrap();
            conversation.rebuild_messages_mirror();
            store.save_conversation(&conversation).await.unwrap();
        }
        let after = store.load_conversation(id).await.unwrap().unwrap();
        assert_eq!(calls(&after), original_calls);
        let (_, rebuilt_frames) =
            acp_notice_turns(build(), &cfg, &ws, Some(&session), &["rebuilt"], false).await;
        if notices {
            for marker in [QUEUED_NOTICE, FLIGHT_NOTICE] {
                let expected = format!("Warning: {marker}");
                assert_eq!(
                    rebuilt_frames
                        .iter()
                        .filter(|v| v["params"]["update"]["content"]["text"] == expected)
                        .count(),
                    1
                );
            }
        }
        let posts = world.posts().await;
        assert_eq!(posts.len(), 6);
        assert_in_turn(&posts[2]);
        let oracle = contents(&posts[2]);
        let final_model =
            json!({"role":"model","parts":[{"text":AUTHORED,"thoughtSignature":SIG[4]}]});
        for (index, prompt) in [(3, "same"), (4, "loaded"), (5, "rebuilt")] {
            let history = contents(&posts[index]);
            assert_eq!(&history[..oracle.len()], oracle.as_slice());
            assert_eq!(history[oracle.len()], final_model);
            assert_eq!(history.last().unwrap(), &user_content(prompt));
            assert_eq!(
                signatures(&Value::Array(history)),
                SIG.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
            );
            assert!(!posts[index].contains(QUEUED_NOTICE));
            assert!(!posts[index].contains(FLIGHT_NOTICE));
            assert!(posts[index].contains(AUTHORED));
        }
        assert_eq!(
            assert_prefix(&posts[3], &posts[4], "contents"),
            vec![
                json!({"role":"model","parts":[{"text":"same answer"}]}),
                user_content("loaded")
            ]
        );
        assert_eq!(
            assert_prefix(&posts[4], &posts[5], "contents"),
            vec![
                json!({"role":"model","parts":[{"text":"loaded answer"}]}),
                user_content("rebuilt")
            ]
        );
        oracle
    }
    let clean = run(false).await;
    let with_notices = run(true).await;
    assert_eq!(with_notices, clean);
}
