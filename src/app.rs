use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;
use std::io::stdout;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::core::provider::{
    AnthropicStreamingProvider, CompletionOptions, Message, StreamingProvider,
};
use crate::tui::ui;
use crate::types::app_state::{AppMode, AppState};
use crate::types::event::{AppEvent, ApprovalDecision};

pub struct App {
    state: AppState,
    should_quit: bool,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    streaming_task: Option<JoinHandle<()>>,
    provider: Option<Box<dyn StreamingProvider>>,
}

impl App {
    pub async fn new() -> Result<Self> {
        let state = AppState::new().await?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Initialize provider if API key available
        let provider: Option<Box<dyn StreamingProvider>> =
            if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
                let base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
                Some(Box::new(AnthropicStreamingProvider::new(api_key, base_url)))
            } else {
                None
            };

        Ok(Self {
            state,
            should_quit: false,
            event_rx,
            event_tx,
            streaming_task: None,
            provider,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        // Panic hook: restore terminal
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = stdout().execute(LeaveAlternateScreen);
            original_hook(info);
        }));

        enable_raw_mode()?;
        stdout().execute(EnterAlternateScreen)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

        // Spawn crossterm event reader
        let tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                    match event::read() {
                        Ok(Event::Key(key)) => {
                            if tx.send(AppEvent::Key(key)).is_err() {
                                break;
                            }
                        }
                        Ok(Event::Resize(w, h)) => {
                            let _ = tx.send(AppEvent::Resize(w, h));
                        }
                        _ => {}
                    }
                }
            }
        });

        // Spawn tick timer
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                if tx.send(AppEvent::Tick).is_err() {
                    break;
                }
            }
        });

        // Set initial status based on provider availability
        if self.provider.is_none() {
            self.state.status_message =
                Some("No ANTHROPIC_API_KEY set. Set it to chat with Claude.".to_string());
        }

        // Main event loop
        loop {
            terminal.draw(|frame| {
                ui::render(frame, &self.state);
            })?;

            // Check for pending send (user pressed Enter)
            if let Some(input) = self.state.pending_send.take() {
                self.start_streaming(input);
            }

            if let Some(event) = self.event_rx.recv().await {
                match event {
                    AppEvent::Key(key) => self.handle_key_event(key),
                    AppEvent::Resize(_w, _h) => {}
                    AppEvent::Stream(stream_event) => {
                        self.state.handle_stream_event(stream_event);
                    }
                    AppEvent::Permission(request) => {
                        self.state.mode = AppMode::PermissionPrompt {
                            tool_name: request.tool_name,
                            tool_input: request.tool_input,
                            pending_tool_id: request.tool_id,
                        };
                        self.state.pending_approval_tx = Some(request.response_tx);
                    }
                    AppEvent::Tick => {}
                }
            }

            if self.should_quit {
                break;
            }
        }

        disable_raw_mode()?;
        stdout().execute(LeaveAlternateScreen)?;
        Ok(())
    }

    /// Start streaming a response from the provider
    fn start_streaming(&mut self, user_input: String) {
        let Some(provider) = &self.provider else {
            self.state.status_message =
                Some("Cannot send: no API key configured.".to_string());
            return;
        };

        // Build message snapshot from conversation
        let tab = self.state.active_tab_mut();
        tab.is_streaming = true;
        let messages: Vec<Message> = tab
            .conversation
            .as_ref()
            .map(|conv| {
                conv.messages
                    .iter()
                    .map(|m| {
                        let role = match m.role {
                            crate::types::conversation::MessageRole::User => "user",
                            crate::types::conversation::MessageRole::Assistant => "assistant",
                            _ => "user",
                        };
                        Message {
                            role: role.to_string(),
                            content: serde_json::json!(m.content),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let options = CompletionOptions {
            model: self.state.model.clone(),
            ..Default::default()
        };

        // Spawn streaming in background task
        let event_tx = self.event_tx.clone();
        // We need to call provider.stream_completion — but provider is behind &self
        // Clone what we need for the task
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        let base_url = std::env::var("ANTHROPIC_BASE_URL").ok();

        let handle = tokio::spawn(async move {
            let provider = AnthropicStreamingProvider::new(api_key, base_url);
            let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();

            // Forward stream events to app
            let fwd_tx = event_tx.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(evt) = stream_rx.recv().await {
                    if fwd_tx.send(AppEvent::Stream(evt)).is_err() {
                        break;
                    }
                }
            });

            let result = provider
                .stream_completion(&messages, &options, &stream_tx)
                .await;

            if let Err(e) = result {
                let _ = stream_tx.send(crate::types::stream::TuiStreamEvent::Error {
                    content: format!("Provider error: {}", e),
                });
            }

            // Ensure Done is sent
            let _ = stream_tx.send(crate::types::stream::TuiStreamEvent::Done);
            drop(stream_tx);
            let _ = forwarder.await;
        });

        self.streaming_task = Some(handle);
    }

    fn handle_key_event(&mut self, key: event::KeyEvent) {
        // Permission prompt mode
        if let AppMode::PermissionPrompt { .. } = &self.state.mode {
            if let Some(tx) = self.state.pending_approval_tx.take() {
                let decision = match key.code {
                    KeyCode::Char('y') => Some(ApprovalDecision::Allow),
                    KeyCode::Char('a') => Some(ApprovalDecision::AlwaysAllow),
                    KeyCode::Char('n') => Some(ApprovalDecision::Deny),
                    KeyCode::Esc => Some(ApprovalDecision::Cancel),
                    _ => None,
                };
                if let Some(decision) = decision {
                    let _ = tx.send(decision);
                    self.state.mode = AppMode::Normal;
                } else {
                    self.state.pending_approval_tx = Some(tx);
                }
            }
            return;
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if self.state.active_tab().is_streaming {
                    if let Some(handle) = self.streaming_task.take() {
                        handle.abort();
                    }
                    self.state.active_tab_mut().is_streaming = false;
                    self.state.status_message = Some("Aborted.".to_string());
                } else {
                    self.should_quit = true;
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                self.should_quit = true;
            }
            _ => {
                self.state.handle_key(key);
            }
        }
    }
}
