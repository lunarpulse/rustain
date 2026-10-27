//! K07: the replay item never reaches an observer, peer, ACP update, `ask`
//! output, TUI render or `tracing` line; only the session store keeps it.
use super::common::*;
use futures::StreamExt;
use rustain::adapters::cli::ask::OutputFormat;
use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::SystemClock;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    ChatMessage, CompletionOptions, Conversation, MessageRole, ReducerState, StreamChunk,
    StreamingState, ViewState, reduce,
};
use rustain::domain::services::message_builder::build_api_messages;
use rustain::infrastructure::runtime::event_bus::{EventBus, RawEvent};
use serial_test::serial;
use std::collections::{BTreeMap, HashMap};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use wiremock::MockServer;

const MARK: &str = "k07";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn render(conversation: &Conversation) -> String {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    let mut terminal = Terminal::new(TestBackend::new(120, 60)).unwrap();
    let clock = SystemClock::default();
    let _ = terminal.draw(|frame| {
        let mut tab_render_state = TabRenderState::default();
        let _ = chat_pane::render_with_search(
            frame,
            Rect::new(0, 0, 120, 60),
            conversation,
            None,
            &StreamingState::default(),
            &ViewState::default(),
            &clock,
            0,
            true,
            &Theme::dark(),
            &mut tab_render_state,
            &HashMap::new(),
            &BTreeMap::new(),
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
        );
    });
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area().height)
        .map(|y| {
            (0..buffer.area().width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
#[serial]
async fn k07_replay_reaches_only_the_session_store() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "k07-account").await;
    mount_script(
        &server,
        vec![sse(&[reasoning(MARK), text("visible answer"), completed()])],
    )
    .await;
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    // Thread-scoped: the current-thread runtime runs every task of this test.
    let trace_guard = tracing::subscriber::set_default(subscriber);

    // `ask` text, JSON and stream-JSON stdout.
    let ws = world.workspace();
    let mut sessions = Vec::new();
    for format in [
        OutputFormat::Text,
        OutputFormat::Json,
        OutputFormat::StreamJson,
    ] {
        let (code, stdout, stderr) = ask(codex(), &ws, opts("hello", format, None)).await;
        assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
        assert!(stdout.contains("visible answer"), "{stdout}");
        assert!(
            !mentions(&stdout, MARK) && !mentions(&stderr, MARK),
            "{stdout}"
        );
        if format == OutputFormat::Json {
            let doc: serde_json::Value = serde_json::from_str(&stdout).unwrap();
            sessions.push(doc["session_id"].as_str().unwrap().to_owned());
        }
    }
    // The persisted session does keep it (replay works).
    assert!(sessions_contain(&ws, &enc(MARK)));

    // The same provider stream through every observer projection.
    let provider = codex();
    let messages = build_api_messages(&Conversation {
        messages: vec![ChatMessage {
            id: "u".into(),
            role: MessageRole::User,
            content: "hello".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let chunks: Vec<StreamChunk> = provider
        .stream_completion(
            messages,
            CompletionOptions {
                model: MODEL.into(),
                max_tokens: 1024,
                system_prompt: String::new(),
                temperature: None,
                tools: vec![],
                session_id: Some("k07".into()),
            },
        )
        .await
        .unwrap()
        .collect()
        .await;
    let replay: Vec<_> = chunks
        .iter()
        .filter(|c| matches!(c, StreamChunk::ProviderReplay { .. }))
        .cloned()
        .collect();
    assert_eq!(replay.len(), 1);
    tracing::info!(chunk = ?replay[0], "replay chunk observed");
    let (bus, _domain_rx) = EventBus::new(64);
    let mut raw = bus.subscribe_raw();
    for chunk in &chunks {
        let provider_event = AppEvent::ProviderChunk {
            conversation_id: "k07".into(),
            chunk: chunk.clone(),
        };
        let peer_event = AppEvent::PeerProviderChunk {
            node: Arc::new(rustain::domain::models::AgentId::new()),
            conversation_id: "k07".into(),
            chunk: chunk.clone(),
        };
        for event in [&provider_event, &peer_event] {
            if let Some(projected) = RawEvent::from_app_event(event) {
                assert!(!mentions(&serde_json::to_string(&projected).unwrap(), MARK));
            }
        }
        let _ = bus.emit_domain(provider_event);
        let acp = rustain::adapters::acp::translate::stream_chunk_to_session_update(
            chunk,
            std::path::Path::new("/workspace"),
        );
        if matches!(chunk, StreamChunk::ProviderReplay { .. }) {
            assert!(acp.is_none(), "replay is never an ACP update");
        }
        if let Some(update) = acp {
            assert!(!mentions(&format!("{update:?}"), MARK));
        }
    }
    let mut observed = 0;
    while let Ok(event) = raw.try_recv() {
        observed += 1;
        assert!(!mentions(&serde_json::to_string(&event).unwrap(), MARK));
    }
    assert_eq!(
        observed,
        chunks.len() - replay.len(),
        "only the replay chunk is withheld"
    );

    // The TUI renders the turn without it.
    let clock = SystemClock::default();
    let mut state = ReducerState::new(0, std::time::Instant::now());
    let mut conversation = Conversation {
        id: "k07".into(),
        ..Default::default()
    };
    for chunk in chunks {
        reduce(&mut state, chunk, &clock);
        if let Some(turn) = state.committed_turn.take() {
            conversation.turns.push(turn);
            conversation.rebuild_messages_mirror();
        }
    }
    assert!(
        serde_json::to_string(&conversation)
            .unwrap()
            .contains(&enc(MARK))
    );
    let screen = render(&conversation);
    assert!(screen.contains("visible answer"), "{screen}");
    assert!(!mentions(&screen, MARK));

    drop(trace_guard);
    let traced = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(traced.contains("replay chunk observed"), "capture is armed");
    assert!(traced.contains("bytes"));
    assert!(!mentions(&traced, MARK), "{traced}");
}
