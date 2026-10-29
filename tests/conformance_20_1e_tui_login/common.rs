//! The in-process TUI the keystones drive: a `TuiState` fed through
//! `handle_input` / `submit_message_for_test`, the event-loop arm handlers
//! (`handlers::login::{command, on_action, on_event}`,
//! `handlers::model_switch::{open_selector, on_catalog_refreshed}`), and a
//! `LiveProviders` built from the production `init_provider_layer`. Rendering
//! goes through the production widgets into a `TestBackend`. Every wait is on
//! an event or a stub request, under a hang guard.
#![allow(dead_code)]

pub(crate) use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    GUARD, Held, NOW, World, accept,
};
use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::tui::app::{InputAction, handle_input, submit_message_for_test};
use rustain::adapters::tui::clipboard::ClipboardResult;
use rustain::adapters::tui::handlers::{login, model_switch};
use rustain::adapters::tui::refresh_tracker::RefreshTracker;
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{hyperlink::LinkSpan, login_dialog, model_selector};
use rustain::domain::events::{AppEvent, DomainInputEvent, DomainKey, LoginEvent};
use rustain::domain::models::{AppConfig, ProviderConfig};
use rustain::infrastructure::live_providers::LiveProviders;
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::startup::init_provider_layer;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedReceiver;

pub(crate) struct Tui {
    pub(crate) state: TuiState,
    pub(crate) live: Arc<LiveProviders>,
    pub(crate) ctx: rustain::adapters::tui::login_runtime::LoginCtx,
    pub(crate) app: AppConfig,
    rx: UnboundedReceiver<AppEvent>,
    /// URLs handed to the browser opener.
    pub(crate) opened: Arc<Mutex<Vec<String>>>,
    /// Texts handed to the clipboard.
    pub(crate) copied: Arc<Mutex<Vec<String>>>,
    /// `Debug` of every event the harness drained (secret-hygiene evidence).
    pub(crate) event_log: Vec<String>,
    /// Every rendered frame's text.
    pub(crate) frames: Vec<String>,
}

impl Tui {
    pub(crate) fn new(app: AppConfig) -> Self {
        let layer = init_provider_layer(&app);
        let (bus, rx) = EventBus::new(256);
        let bus = Arc::new(bus);
        let tracker = RefreshTracker::new();
        let live = LiveProviders::new(
            layer.router,
            layer.registry,
            layer.discovery_targets,
            layer.catalogue_targets,
            bus.clone(),
            tracker.clone(),
            app.clone(),
        );
        let mut state = TuiState::new(80, 24);
        state.model_selector.refreshing = tracker;
        let opened = Arc::new(Mutex::new(Vec::new()));
        let copied = Arc::new(Mutex::new(Vec::new()));
        let (open_log, copy_log) = (opened.clone(), copied.clone());
        let ctx = rustain::adapters::tui::login_runtime::LoginCtx::new(bus, live.clone())
            .with_opener(Arc::new(move |url: &str| {
                open_log.lock().unwrap().push(url.to_owned())
            }))
            .with_copier(Arc::new(move |text: &str| {
                copy_log.lock().unwrap().push(text.to_owned());
                ClipboardResult::Osc52Success
            }))
            .with_clock(Arc::new(|| NOW));
        Self {
            state,
            live,
            ctx,
            app,
            rx,
            opened,
            copied,
            event_log: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// The event loop's handling of an `InputAction` the tests need.
    pub(crate) fn dispatch(&mut self, action: InputAction) -> Option<InputAction> {
        match action {
            InputAction::Login(action) => {
                login::on_action(&mut self.state, &self.ctx, &self.app, &*self.live, action);
                None
            }
            InputAction::OpenModelSelector => {
                model_switch::open_selector(&mut self.state, &*self.live, &self.app, None);
                None
            }
            InputAction::ExecuteCommand { name, args } if name == "login" => {
                login::command(&mut self.state, &self.ctx, &self.app, args.as_deref());
                None
            }
            InputAction::ExecuteCommand { name, .. } if name == "models" => {
                model_switch::open_selector(&mut self.state, &*self.live, &self.app, None);
                None
            }
            other => Some(other),
        }
    }

    pub(crate) fn input(&mut self, event: DomainInputEvent) -> Option<InputAction> {
        let action = handle_input(&mut self.state, &event);
        self.dispatch(action)
    }
    pub(crate) fn key(&mut self, key: DomainKey) -> Option<InputAction> {
        self.input(DomainInputEvent::SpecialKey(key))
    }
    pub(crate) fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.input(DomainInputEvent::KeyPress(c));
        }
    }
    pub(crate) fn paste(&mut self, text: &str) {
        self.input(DomainInputEvent::Paste(text.into()));
    }
    /// Submits a slash command through `submit_message` and its arm.
    pub(crate) fn submit(&mut self, line: &str) -> InputAction {
        self.state.input_buffer = line.into();
        self.state.cursor_position = line.chars().count();
        let action = submit_message_for_test(&mut self.state);
        let shown = format!("{action:?}");
        if let InputAction::ExecuteCommand { name, args } = &action {
            let copy = InputAction::ExecuteCommand {
                name: name.clone(),
                args: args.clone(),
            };
            self.dispatch(copy);
        }
        let _ = shown;
        action
    }

    /// Applies one event as the event loop would; returns it for inspection.
    pub(crate) fn apply(&mut self, event: AppEvent) {
        self.event_log.push(format!("{event:?}"));
        match event {
            AppEvent::Login(event) => {
                login::on_event(&mut self.state, &self.app, &*self.live, event)
            }
            AppEvent::ProviderCatalogRefreshed { .. } => {
                model_switch::on_catalog_refreshed(&mut self.state, &*self.live, &self.app)
            }
            _ => {}
        }
    }

    /// Drains events until `done` holds (hang guard only).
    pub(crate) async fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        while !done(self) {
            let event = tokio::time::timeout(GUARD, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("hang guard waiting for {what}"))
                .expect("event bus open");
            self.apply(event);
        }
    }

    /// Applies every event already queued.
    pub(crate) fn drain(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply(event);
        }
    }

    /// Waits for the login event matching `matches`, applying everything before it.
    pub(crate) async fn until_login(&mut self, what: &str, matches: impl Fn(&LoginEvent) -> bool) {
        loop {
            let event = tokio::time::timeout(GUARD, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("hang guard waiting for {what}"))
                .expect("event bus open");
            let hit = matches!(&event, AppEvent::Login(login) if matches(login));
            self.apply(event);
            if hit {
                return;
            }
        }
    }

    /// The running view has a pending field.
    pub(crate) fn field_pending(&self) -> bool {
        self.state
            .login
            .running
            .as_ref()
            .is_some_and(|running| running.field.is_some())
    }

    /// Renders both overlays at `width`×`height`; returns the text and the
    /// links the event loop would write over the frame.
    pub(crate) fn frame_at(&mut self, width: u16, height: u16) -> (String, Vec<LinkSpan>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::dark();
        let mut links = Vec::new();
        terminal
            .draw(|frame| {
                model_selector::render(frame, frame.area(), &self.state.model_selector, &theme);
                links = login_dialog::render(frame, frame.area(), &self.state.login, &theme);
            })
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        self.frames.push(text.clone());
        (text, links)
    }
    pub(crate) fn frame(&mut self) -> String {
        self.frame_at(80, 24).0
    }

    pub(crate) fn feedback(&self) -> String {
        self.state
            .feedback_blocks
            .values()
            .map(|block| block.message.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub(crate) fn flash(&self) -> String {
        match &self.state.status {
            rustain::domain::models::StatusState::Flash { message, .. } => message.clone(),
            _ => String::new(),
        }
    }
}

/// One text line per buffer row.
pub(crate) fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// The `[provider.stubai]` entry of the 20-1c smoke form: a full
/// openai-compatible entry that discovers its live roster.
pub(crate) fn stubai(base: &str) -> ProviderConfig {
    ProviderConfig {
        provider_id: "stubai".into(),
        model_id: "stub-a".into(),
        api_key_env: "STUBAI_API_KEY".into(),
        enabled: true,
        kind: Some("openai-compatible".into()),
        base_url: Some(format!("{base}/stubai/v1").into()),
        auth_header: None,
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 0,
    }
}

pub(crate) fn config_with(entries: &[(&str, ProviderConfig)]) -> AppConfig {
    let mut app = AppConfig::default();
    for (id, cfg) in entries {
        app.provider.insert((*id).into(), cfg.clone());
    }
    app
}

/// An OpenAI-style `/models` listing.
pub(crate) fn listing(ids: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "object": "list",
        "data": ids.iter().map(|id| serde_json::json!({"id": id, "object": "model"})).collect::<Vec<_>>(),
    })
}

/// Answers a held chat request with one streamed `text` and `[DONE]`.
pub(crate) async fn reply_sse_text(held: Held, text: &str) {
    let chunk = serde_json::json!({
        "id": "c1", "object": "chat.completion.chunk", "model": "stub-b",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}],
    });
    let done = serde_json::json!({
        "id": "c1", "object": "chat.completion.chunk", "model": "stub-b",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
    });
    let body = format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n");
    let mut stream = held.stream;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.ok();
}

/// Contents of the clipboard fallback file, if any was written.
pub(crate) fn clipboard_file(world: &World) -> String {
    std::fs::read_to_string(world.dir.path().join("clipboard.txt")).unwrap_or_default()
}
