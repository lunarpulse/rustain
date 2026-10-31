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
use rustain::adapters::tui::handlers::{
    AuthWork, HandlerOutcome, SpawnRequest, accounts, login, model_switch,
};
use rustain::adapters::tui::refresh_tracker::RefreshTracker;
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{login_dialog, model_selector};
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
                let outcome = login::on_action(
                    &mut self.state,
                    &self.ctx.copier,
                    &self.app,
                    &*self.live,
                    action,
                );
                self.consume(outcome);
                None
            }
            InputAction::OpenModelSelector => {
                model_switch::open_selector(&mut self.state, &*self.live, &self.app, None);
                None
            }
            InputAction::ExecuteCommand { name, args } if name == "login" => {
                let outcome = login::command(&mut self.state, &self.app, args.as_deref());
                self.consume(outcome);
                None
            }
            InputAction::ExecuteCommand { name, args }
                if name == "logout" || name == "accounts" =>
            {
                let outcome = accounts::command(&mut self.state, &name, args.as_deref());
                self.consume(outcome);
                None
            }
            InputAction::ExecuteCommand { name, .. } if name == "models" => {
                model_switch::open_selector(&mut self.state, &*self.live, &self.app, None);
                None
            }
            other => Some(other),
        }
    }

    /// Mirror dispatch ownership: bind login state before launching, and fan out
    /// picker validation only after the coordinator has published its rows.
    fn consume(&mut self, outcome: HandlerOutcome) {
        use rustain::adapters::tui::state::RunningLogin;
        use rustain::adapters::tui::{accounts_runtime, login_runtime, login_ui::TuiLoginUi};
        use rustain::domain::models::{FocusState, visual::OverlayType};
        use tokio_util::sync::CancellationToken;

        let HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(work)) = outcome else {
            return;
        };
        let ctx = self.ctx.clone();
        let app = self.app.clone();
        match work {
            AuthWork::Login {
                login_id,
                plan,
                method,
                origin,
            } => {
                let cancel = CancellationToken::new();
                let ui = Arc::new(TuiLoginUi::new(
                    ctx.events.clone(),
                    login_id,
                    cancel.clone(),
                    ctx.opener.clone(),
                ));
                self.state.login.origin = origin.clone();
                self.state.login.start_running(RunningLogin {
                    login_id,
                    provider_id: plan.provider_id.clone(),
                    display: rustain::adapters::auth::bounded_provider_text(&plan.display),
                    origin,
                    cancel,
                    sections: Vec::new(),
                    progress: Vec::new(),
                    field: None,
                });
                self.state.focus = FocusState::Overlay(OverlayType::Login);
                self.state.needs_redraw = true;
                tokio::spawn(login_runtime::run_login(
                    ctx, app, login_id, plan, method, ui,
                ));
            }
            AuthWork::Targets { generation } => {
                tokio::spawn(async move {
                    let checks = login_runtime::prepare_targets(ctx.clone(), app, generation).await;
                    for check in checks {
                        tokio::spawn(login_runtime::validate_target(
                            ctx.clone(),
                            generation,
                            check,
                        ));
                    }
                });
            }
            AuthWork::Providers { generation } => {
                tokio::spawn(accounts_runtime::run_providers(ctx, app, generation));
            }
            AuthWork::Listing {
                generation,
                mode,
                provider,
            } => {
                tokio::spawn(accounts_runtime::run_listing(
                    ctx, app, generation, mode, provider,
                ));
            }
            AuthWork::Change {
                generation,
                change,
                provider,
                id,
            } => {
                tokio::spawn(accounts_runtime::run_change(
                    ctx, app, generation, change, provider, id,
                ));
            }
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

    /// Renders both overlays with deterministic plain-link capability.
    pub(crate) fn frame_at(&mut self, width: u16, height: u16) -> String {
        let buffer = self.login_buffer_at(width, height, false);
        let text = buffer_text(&buffer);
        self.frames.push(text.clone());
        text
    }

    pub(crate) fn login_buffer_at(
        &self,
        width: u16,
        height: u16,
        supports_osc8: bool,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::dark();
        terminal
            .draw(|frame| {
                model_selector::render(frame, frame.area(), &self.state.model_selector, &theme);
                login_dialog::render(
                    frame,
                    frame.area(),
                    &self.state.login,
                    &theme,
                    supports_osc8,
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }
    pub(crate) fn frame(&mut self) -> String {
        self.frame_at(80, 24)
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

/// `buffer_text` with every OSC 8 envelope reduced to its one visible glyph.
pub(crate) fn decoded_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            let symbol = buffer[(x, y)].symbol();
            match symbol
                .strip_prefix("\x1b]8;;")
                .and_then(|rest| rest.split_once('\x07'))
                .and_then(|(_, rest)| rest.strip_suffix("\x1b]8;;\x07"))
            {
                Some(glyph) => out.push_str(glyph),
                None => out.push_str(symbol),
            }
        }
        out.push('\n');
    }
    out
}

/// Inspect the supported cells of the login overlay. Only the exact balanced
/// per-glyph envelope is accepted, and the enveloped cells must be exactly the
/// URL glyphs of each target: the text after its label on a one-row link, or
/// every glyph of the rows right after a wrapped link's label row. A label with
/// no enveloped row after it is a label-only link. The plain buffer carries no
/// envelope. Returns each target's retained URL-row count (0 = label only).
pub(crate) fn assert_link_cells(
    tui: &Tui,
    width: u16,
    height: u16,
    targets: &[(&str, &str)],
) -> Vec<usize> {
    use ratatui::buffer::CellDiffOption;
    use ratatui::style::Modifier;
    let plain = tui.login_buffer_at(width, height, false);
    let linked = tui.login_buffer_at(width, height, true);
    let theme = Theme::dark();
    let (width, height) = (width as usize, height as usize);
    let mut cells = vec![vec![String::new(); width]; height];
    let mut linked_to: Vec<Vec<Option<String>>> = vec![vec![None; width]; height];
    for y in 0..height {
        for x in 0..width {
            let cell = &linked[(x as u16, y as u16)];
            let fallback = &plain[(x as u16, y as u16)];
            assert!(
                !fallback.symbol().chars().any(char::is_control),
                "plain cell at {x},{y} carries a control"
            );
            let symbol = cell.symbol();
            if let Some(rest) = symbol.strip_prefix("\x1b]8;;") {
                let (target, rest) = rest.split_once('\x07').expect("OSC target terminator");
                let glyph = rest
                    .strip_suffix("\x1b]8;;\x07")
                    .expect("balanced OSC close");
                assert_eq!(glyph.chars().count(), 1, "one glyph per envelope");
                assert!(!glyph.chars().any(char::is_control));
                assert_eq!(cell.fg, theme.colors.accent);
                assert_eq!(cell.bg, theme.colors.bg_surface);
                assert!(cell.modifier.contains(Modifier::BOLD));
                assert_eq!(
                    cell.diff_option,
                    CellDiffOption::ForcedWidth(std::num::NonZeroU16::new(1).unwrap())
                );
                cells[y][x] = glyph.to_owned();
                linked_to[y][x] = Some(target.to_owned());
            } else {
                assert!(!symbol.chars().any(char::is_control), "control at {x},{y}");
                cells[y][x] = symbol.to_owned();
            }
        }
    }
    let envelope_xs =
        |y: usize| -> Vec<usize> { (0..width).filter(|x| linked_to[y][*x].is_some()).collect() };
    let rows: Vec<String> = cells.iter().map(|row| row.concat()).collect();
    let mut claimed = 0;
    let mut counts = Vec::new();
    for (label, target) in targets {
        let needle = label.trim_end();
        let found: Vec<usize> = (0..height).filter(|y| rows[*y].contains(needle)).collect();
        assert_eq!(found.len(), 1, "one visible row for {label:?}");
        let y = found[0];
        let label_x = rows[y][..rows[y].find(needle).unwrap()].chars().count();
        let left = rows[y].find('│').map(|at| rows[y][..at].chars().count());
        let url_start = label_x + label.chars().count();
        let one_row = envelope_xs(y);
        if !one_row.is_empty() {
            let wanted: Vec<usize> = (url_start..url_start + target.chars().count()).collect();
            assert_eq!(one_row, wanted, "a one-row link covers exactly its value");
            assert_eq!(
                wanted
                    .iter()
                    .map(|x| cells[y][*x].as_str())
                    .collect::<String>(),
                *target
            );
            assert!(
                wanted
                    .iter()
                    .all(|x| linked_to[y][*x].as_deref() == Some(*target))
            );
            claimed += wanted.len();
            counts.push(1);
            continue;
        }
        let x0 = left.expect("modal left border") + 1;
        let mut joined = String::new();
        let mut count = 0;
        let mut at = y + 1;
        while at < height
            && joined.chars().count() < target.chars().count()
            && envelope_xs(at).first() == Some(&x0)
        {
            let xs = envelope_xs(at);
            let wanted: Vec<usize> = (x0..x0 + xs.len()).collect();
            assert_eq!(xs, wanted, "URL rows start at the inner left edge");
            assert!(
                xs.iter()
                    .all(|x| linked_to[at][*x].as_deref() == Some(*target))
            );
            let right = rows[at]
                .rfind('│')
                .map(|index| rows[at][..index].chars().count())
                .expect("modal right border");
            assert!(
                x0 + xs.len() < right,
                "a blank cell stays before the right border"
            );
            joined.extend(xs.iter().map(|x| cells[at][*x].as_str()));
            claimed += xs.len();
            count += 1;
            at += 1;
        }
        assert!(
            joined.is_empty() || joined == *target,
            "the retained rows rejoin to the full URL or none: {joined:?}"
        );
        counts.push(count);
    }
    let total: usize = (0..height).map(|y| envelope_xs(y).len()).sum();
    assert_eq!(total, claimed, "an envelope outside every link");
    if !targets.is_empty() && counts.iter().all(|count| *count == 1) {
        assert_eq!(
            rows.join("\n") + "\n",
            buffer_text(&plain),
            "one-row links read the same"
        );
        for y in 0..height {
            for x in 0..width {
                assert_eq!(
                    linked[(x as u16, y as u16)].style(),
                    plain[(x as u16, y as u16)].style(),
                    "style at {x},{y}"
                );
            }
        }
    }
    counts
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
