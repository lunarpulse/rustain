//! K01 (local replay and reload), K03 (retry boundary) and the E1 plain tests.
use super::common::*;
use futures::StreamExt;
use rustain::adapters::cli::ask::OutputFormat;
use rustain::adapters::responses::events::EventMapper;
use rustain::domain::clock::SystemClock;
use rustain::domain::models::{
    ChatMessage, CompletionOptions, ContentBlockType, Conversation, MessageRole, ProviderConfig,
    ReducerState, StopReason, StreamChunk, reduce,
};
use rustain::domain::ports::{StoragePort, StreamingProvider};
use rustain::domain::services::message_builder::build_api_messages;
use serde_json::{Value, json};
use serial_test::serial;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn options(model: &str, session: &str) -> CompletionOptions {
    CompletionOptions {
        model: model.into(),
        max_tokens: 4096,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
        session_id: Some(session.into()),
    }
}

async fn collect(
    provider: &Arc<dyn StreamingProvider>,
    messages: Vec<rustain::domain::models::Message>,
    options: CompletionOptions,
) -> Vec<StreamChunk> {
    provider
        .stream_completion(messages, options)
        .await
        .unwrap()
        .collect()
        .await
}

fn replay_chunks(chunks: &[StreamChunk]) -> usize {
    chunks
        .iter()
        .filter(|c| matches!(c, StreamChunk::ProviderReplay { .. }))
        .count()
}

/// Fold chunks through the TUI reducer and its persisted message mirror.
fn fold(conversation: &mut Conversation, chunks: Vec<StreamChunk>) {
    let clock = SystemClock::default();
    let mut state = ReducerState::new(0, std::time::Instant::now());
    for chunk in chunks {
        reduce(&mut state, chunk, &clock);
        if let Some(turn) = state.committed_turn.take() {
            conversation.turns.push(turn);
            conversation.rebuild_messages_mirror();
        }
    }
}

fn user(text: &str) -> ChatMessage {
    ChatMessage {
        id: format!("user-{text}"),
        role: MessageRole::User,
        content: text.into(),
        ..Default::default()
    }
}

#[tokio::test]
#[serial]
async fn k01_ask_tool_round_replays_before_function_call_and_after_session_resume() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "k01-account").await;
    mount_script(
        &server,
        vec![
            sse(&[reasoning("k01"), tool_call("call-k01"), completed()]),
            sse(&[text("first answer"), completed()]),
            sse(&[text("resumed answer"), completed()]),
        ],
    )
    .await;
    let ws = world.workspace();
    let (code, stdout, stderr) = ask(codex(), &ws, opts("first", OutputFormat::Json, None)).await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(doc["response"], "first answer");
    assert!(!mentions(&stdout, "k01") && !mentions(&stderr, "k01"));
    let bodies = posts(&server).await;
    assert_eq!(bodies.len(), 2);
    assert!(replayed(&bodies[0]).is_empty());
    // In-turn keeper: the item precedes the call it reasoned about.
    assert_replays(&bodies[1], "k01");
    let item = position(&bodies[1], |i| i["type"] == "reasoning").unwrap();
    let call = position(&bodies[1], |i| i["type"] == "function_call").unwrap();
    assert!(item < call, "reasoning must precede its function_call");

    // Persisted on the assistant row, then replayed after a session resume.
    let session = doc["session_id"].as_str().unwrap().to_owned();
    let stored = storage(&ws)
        .load_conversation(&session)
        .await
        .unwrap()
        .unwrap();
    let blocks: Vec<_> = stored
        .messages
        .iter()
        .flat_map(|m| m.content_blocks.iter())
        .filter_map(|b| match b {
            ContentBlockType::ProviderReplay(item) => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].provider, "openai-codex");
    assert_eq!(blocks[0].model, MODEL);
    assert_eq!(blocks[0].payload["encrypted_content"], enc("k01"));
    let (code, _, stderr) = ask(
        codex(),
        &ws,
        opts("second", OutputFormat::Json, Some(session.clone())),
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    let bodies = posts(&server).await;
    assert_eq!(bodies.len(), 3);
    assert_replays(&bodies[2], "k01");
    let item = position(&bodies[2], |i| i["type"] == "reasoning").unwrap();
    let call = position(&bodies[2], |i| i["type"] == "function_call").unwrap();
    assert!(item < call);

    // Another Codex model never receives the item.
    let mut other = opts("third", OutputFormat::Json, Some(session));
    other.model_override = Some("gpt-6.1-sol".into());
    let (code, _, stderr) = ask(codex(), &ws, other).await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    let bodies = posts(&server).await;
    assert_eq!(bodies[3]["model"], "gpt-6.1-sol");
    assert!(replayed(&bodies[3]).is_empty());
    assert!(!mentions(&bodies[3], "k01"));
}

#[tokio::test]
#[serial]
async fn k01_reducer_persists_replay_across_local_reload_and_never_leaves_codex() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "k01-local").await;
    mount_script(
        &server,
        vec![
            sse(&[reasoning("local"), text("visible answer"), completed()]),
            sse(&[text("again"), completed()]),
        ],
    )
    .await;
    let provider = codex();
    let first = vec![
        build_api_messages(&{
            let mut c = Conversation::default();
            c.messages.push(user("hello"));
            c
        })[0]
            .clone(),
    ];
    let chunks = collect(&provider, first, options(MODEL, "local")).await;
    assert_eq!(replay_chunks(&chunks), 1);
    // Released after the output and before the terminal stop.
    let replay_at = chunks
        .iter()
        .position(|c| matches!(c, StreamChunk::ProviderReplay { .. }))
        .unwrap();
    let complete_at = chunks
        .iter()
        .position(|c| matches!(c, StreamChunk::TurnComplete { .. }))
        .unwrap();
    assert!(replay_at < complete_at);

    let mut conversation = Conversation {
        id: "k01-local".into(),
        ..Default::default()
    };
    conversation.messages.push(user("hello"));
    fold(&mut conversation, chunks);
    // A session reload is a serde round trip of the conversation file.
    let saved = serde_json::to_string(&conversation).unwrap();
    assert!(
        saved.contains(&enc("local")),
        "replay state must persist locally"
    );
    let mut reloaded: Conversation = serde_json::from_str(&saved).unwrap();
    reloaded.messages.push(user("next"));
    let messages = build_api_messages(&reloaded);
    collect(&provider, messages.clone(), options(MODEL, "local")).await;
    let bodies = posts(&server).await;
    assert_replays(&bodies[1], "local");
    let item = position(&bodies[1], |i| i["type"] == "reasoning").unwrap();
    let output = position(&bodies[1], |i| i["role"] == "assistant").unwrap();
    assert!(item < output, "reasoning precedes that turn's output");

    // An OpenAI Chat request carries none of it.
    world.set("OPENAI_API_KEY", "sk-k01-chat");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ))
        .mount(&server)
        .await;
    let chat = rustain::infrastructure::provider_factory::build_provider_for_config(
        "openai",
        &ProviderConfig {
            provider_id: "openai".into(),
            kind: Some("openai".into()),
            model_id: MODEL.into(),
            api_key_env: "OPENAI_API_KEY".into(),
            auth_header: None,
            enabled: true,
            base_url: Some(format!("{}/v1", server.uri()).into()),
            context_window: None,
            supports_tools: None,
            discover_models: false,
            model_filter: vec![],
            cache_ttl_seconds: 3600,
        },
    )
    .unwrap();
    if let Ok(stream) = chat
        .stream_completion(messages, options(MODEL, "chat"))
        .await
    {
        let _: Vec<_> = stream.collect().await;
    }
    let chat_bodies: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/chat/completions"))
        .collect();
    assert_eq!(chat_bodies.len(), 1);
    assert!(!String::from_utf8_lossy(&chat_bodies[0].body).contains("ENC-local"));

    // A compaction side request over the same history carries none either.
    let (domain_tx, _domain_rx) = tokio::sync::mpsc::unbounded_channel();
    let history_text = rustain::domain::services::compaction::build_compaction_prompt_input(
        &reloaded, "", 1_000_000,
    );
    assert!(!mentions(&history_text, "local"));
    rustain::adapters::tui::handlers::compaction::run_compaction(
        rustain::adapters::tui::handlers::CompactionPayload {
            provider: provider.clone(),
            model: MODEL.into(),
            history_text,
            conversation_id: "k01-local".into(),
            first_kept_message_id: None,
            pre_tokens: 0,
            purpose: rustain::domain::events::CompactionPurpose::Carryover,
            domain_tx,
        },
    )
    .await;
    let bodies = posts(&server).await;
    assert_eq!(bodies.len(), 3);
    assert!(replayed(&bodies[2]).is_empty());
    assert!(!mentions(&bodies[2], "local"));
}

/// One `ask` whose first attempt fails before commit; the retried attempt and
/// the follow-up request are answered by `retried` and `follow_up`.
async fn retry_case(first: Vec<Value>, discarded: &str, kept: &str) {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "k03-account").await;
    mount_script(
        &server,
        vec![
            sse(&first),
            sse(&[reasoning(kept), tool_call("call-k03"), completed()]),
            sse(&[text("after tools"), completed()]),
        ],
    )
    .await;
    let ws = world.workspace();
    let (code, stdout, stderr) = ask(codex(), &ws, opts("go", OutputFormat::Json, None)).await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    let bodies = posts(&server).await;
    assert_eq!(bodies.len(), 3, "one safe re-POST, then the tool round");
    assert_eq!(bodies[0], bodies[1], "the re-POST resends the same body");
    assert_replays(&bodies[2], kept);
    assert_eq!(replayed(&bodies[2]).len(), 1);
    assert!(!mentions(&bodies[2], discarded));
    assert!(sessions_contain(&ws, &enc(kept)));
    assert!(!sessions_contain(&ws, &enc(discarded)));
    assert!(!mentions(&stdout, kept) && !mentions(&stdout, discarded));
}

#[tokio::test]
#[serial]
async fn k03_failed_attempt_reposts_and_only_the_retried_item_persists() {
    // A reasoning item alone never closes the safe-retry window.
    retry_case(vec![reasoning("k03a-A"), failed()], "k03a-A", "k03a-B").await;
}

#[tokio::test]
#[serial]
async fn k03_missing_terminal_reposts_and_only_the_completed_item_persists() {
    retry_case(vec![reasoning("k03b-A")], "k03b-A", "k03b-C").await;
}

#[tokio::test]
#[serial]
async fn k03_whitespace_loop_reposts_and_only_the_replayed_attempt_persists() {
    let mut first = vec![reasoning("k03c-A")];
    first.extend(whitespace_loop());
    retry_case(first, "k03c-A", "k03c-D").await;
}

#[tokio::test]
#[serial]
async fn k03_committed_then_failed_stream_never_persists_its_item() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "k03d-account").await;
    mount_script(
        &server,
        vec![sse(&[text("visible"), reasoning("k03d-E"), failed()])],
    )
    .await;
    // Observed at the transport: the attempt ends in an error, never re-POSTs and
    // releases no replay chunk (`ask` below saves nothing after an error anyway).
    let chunks = collect(
        &codex(),
        build_api_messages(&{
            let mut c = Conversation::default();
            c.messages.push(user("go"));
            c
        }),
        options(MODEL, "committed"),
    )
    .await;
    assert!(matches!(chunks.last(), Some(StreamChunk::Error { .. })));
    assert_eq!(replay_chunks(&chunks), 0);
    assert_eq!(
        posts(&server).await.len(),
        1,
        "committed output is never re-POSTed"
    );
    let ws = world.workspace();
    let (code, stdout, stderr) = ask(codex(), &ws, opts("go", OutputFormat::Json, None)).await;
    assert_ne!(code, ExitCode::SUCCESS);
    assert_eq!(
        posts(&server).await.len(),
        2,
        "committed output is never re-POSTed"
    );
    assert!(!sessions_contain(&ws, &enc("k03d-E")));
    assert!(!mentions(&stdout, "k03d-E") && !mentions(&stderr, "k03d-E"));
}

#[tokio::test]
#[serial]
async fn k03_cancel_mid_stream_after_an_item_never_persists_it() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "k03e-account").await;
    // The held stream lives on a raw loopback stub at the instrumented origin.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    world.set(
        "RUSTAIN_TEST_AUTH_ORIGIN",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let provider = codex();
    let server_side = tokio::spawn(async move {
        let mut held = accept(&listener).await;
        assert_eq!(held.path(), RESPONSE_PATH);
        let data = format!(
            "data: {}\n\ndata: {}\n\n",
            reasoning("k03e-F"),
            text("partial")
        );
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        held.stream.write_all(head.as_bytes()).await.unwrap();
        held.stream
            .write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes())
            .await
            .unwrap();
        held.stream.flush().await.unwrap();
        held
    });
    let mut stream = provider
        .stream_completion(
            build_api_messages(&{
                let mut c = Conversation::default();
                c.messages.push(user("go"));
                c
            }),
            options(MODEL, "cancel"),
        )
        .await
        .unwrap();
    let mut chunks = Vec::new();
    // Item F precedes the text delta on the wire: receiving the text proves the
    // mapper already saw F.
    while let Some(chunk) = tokio::time::timeout(GUARD, stream.next()).await.unwrap() {
        let is_text = matches!(chunk, StreamChunk::Text { .. });
        chunks.push(chunk);
        if is_text {
            break;
        }
    }
    let _held = server_side.await.unwrap();
    provider.abort().await.unwrap();
    while let Some(chunk) = tokio::time::timeout(GUARD, stream.next()).await.unwrap() {
        chunks.push(chunk);
    }
    assert!(matches!(
        chunks.last(),
        Some(StreamChunk::TurnComplete {
            stop_reason: StopReason::Cancelled
        })
    ));
    assert_eq!(replay_chunks(&chunks), 0);
    let mut conversation = Conversation::default();
    conversation.messages.push(user("go"));
    fold(&mut conversation, chunks);
    assert!(!mentions(
        &serde_json::to_string(&conversation).unwrap(),
        "k03e-F"
    ));
}

// ── plain tests ─────────────────────────────────────────────────────────────

fn events(mapper: &mut EventMapper, events: &[Value]) -> Vec<StreamChunk> {
    events
        .iter()
        .flat_map(|e| mapper.event(e.clone()).unwrap())
        .collect()
}

#[test]
fn null_encrypted_content_and_multipart_summary_are_kept_verbatim() {
    let mut mapper = EventMapper::with_replay("openai-codex", MODEL);
    let item = json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1",
        "summary":[{"type":"summary_text","text":"one"},{"type":"summary_text","text":"two"}],
        "content":[{"type":"reasoning_text","text":"raw"}],"encrypted_content":null,"status":"completed"}});
    let chunks = events(&mut mapper, &[item, completed()]);
    let StreamChunk::ProviderReplay { item } = &chunks[0] else {
        panic!("replay first: {chunks:?}")
    };
    assert_eq!(item.provider, "openai-codex");
    assert_eq!(item.model, MODEL);
    assert_eq!(
        *item.payload,
        json!({"type":"reasoning","summary":[{"type":"summary_text","text":"one"},{"type":"summary_text","text":"two"}],
            "content":[{"type":"reasoning_text","text":"raw"}],"encrypted_content":null})
    );
}

#[test]
fn non_reasoning_items_and_neutral_mappers_capture_nothing() {
    let message = json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","content":[]}});
    let mut mapper = EventMapper::with_replay("openai-codex", MODEL);
    assert_eq!(
        replay_chunks(&events(&mut mapper, &[message, completed()])),
        0
    );
    let mut neutral = EventMapper::new();
    assert_eq!(
        replay_chunks(&events(&mut neutral, &[reasoning("n"), completed()])),
        0
    );
}

#[test]
fn reasoning_flushes_only_at_terminal_after_tools_and_before_usage() {
    let mut mapper = EventMapper::with_replay("openai-codex", MODEL);
    assert!(mapper.event(reasoning("order")).unwrap().is_empty());
    assert!(
        !mapper.committed(),
        "private state never closes the retry window"
    );
    let pending = events(
        &mut mapper,
        &[
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_p","call_id":"call-p","name":"lookup"}}),
            json!({"type":"response.function_call_arguments.done","item_id":"fc_p","arguments":"{}"}),
        ],
    );
    assert!(pending.is_empty());
    let chunks = mapper.event(completed()).unwrap();
    let kinds: Vec<_> = chunks
        .iter()
        .map(|c| match c {
            StreamChunk::ToolUse { .. } => "tool",
            StreamChunk::ProviderReplay { .. } => "replay",
            StreamChunk::Usage { .. } => "usage",
            StreamChunk::TurnComplete { .. } => "complete",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["tool", "replay", "usage", "complete"]);
    // A failed terminal releases nothing.
    let mut failed_mapper = EventMapper::with_replay("openai-codex", MODEL);
    assert!(failed_mapper.event(reasoning("lost")).unwrap().is_empty());
    assert!(failed_mapper.event(failed()).is_err());
}

#[test]
fn replay_item_debug_omits_the_payload() {
    let mut mapper = EventMapper::with_replay("openai-codex", MODEL);
    let chunks = events(&mut mapper, &[reasoning("debug"), completed()]);
    let rendered = format!("{chunks:?}");
    assert!(!mentions(&rendered, "debug"), "{rendered}");
    assert!(rendered.contains("openai-codex") && rendered.contains("bytes"));
}
