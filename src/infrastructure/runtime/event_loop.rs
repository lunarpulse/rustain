use std::sync::Arc;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::adapters::tui::app::{convert_crossterm_event, handle_input};
use crate::adapters::tui::color_detect::detect_color_capability;
use crate::adapters::tui::layout;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::terminal::Tui;
use crate::adapters::tui::widgets::{chat_pane, input_box, status_bar};
use crate::domain::events::{AppEvent, ChunkAction};
use crate::domain::models::{
    AppConfig, CompletionOptions, Conversation, Message, MessageRole, StreamingState, apply_chunk,
    generate_conversation_id,
};
use crate::domain::ports::ProviderPort;
use crate::infrastructure::runtime::turn;

/// Run the 4-branch tokio::select! event loop.
pub async fn run(
    terminal: &mut Tui,
    domain_events_rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    config: &AppConfig,
    provider: Arc<dyn ProviderPort>,
) -> Result<()> {
    let size = terminal.size()?;
    let capability = detect_color_capability();
    let mut state = TuiState::with_capability(size.width, size.height, capability);

    let mut terminal_events = EventStream::new();
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(
        state.theme.timing.tick_interval_ms,
    ));

    // Conversation and streaming state for MVP single-tab
    let mut conversation = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: Vec::new(),
        created_at: now_unix(),
        updated_at: now_unix(),
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    };
    let mut streaming = StreamingState::default();

    // Track active turn task for cancellation
    let mut _active_turn: Option<tokio::task::JoinHandle<()>> = None;

    // Render first frame immediately
    render(terminal, &state, &config.model)?;
    state.needs_redraw = false;

    loop {
        tokio::select! {
            // Branch 1: Terminal input (crossterm event stream)
            Some(event_result) = terminal_events.next() => {
                match event_result {
                    Ok(event) => {
                        if is_ctrl_c(&event) {
                            break;
                        }
                        if let Some(domain_event) = convert_crossterm_event(&event) {
                            // Check if Enter was pressed with input text (message submission)
                            let submitted_text = check_message_submit(&state, &domain_event);
                            handle_input(&mut state, &domain_event);

                            if let Some(text) = submitted_text {
                                if !streaming.is_streaming {
                                    // Add user ChatMessage to conversation
                                    conversation.messages.push(
                                        crate::domain::models::ChatMessage {
                                            role: MessageRole::User,
                                            content: text,
                                            content_blocks: vec![],
                                            tool_calls: vec![],
                                            created_at: now_unix(),
                                            token_count: None,
                                        },
                                    );

                                    // Build messages list for provider
                                    let messages: Vec<Message> = conversation
                                        .messages
                                        .iter()
                                        .map(|cm| Message {
                                            role: cm.role,
                                            content: cm.content.clone(),
                                            images: vec![],
                                            tool_results: vec![],
                                            context_prefix: None,
                                        })
                                        .collect();

                                    let options = CompletionOptions {
                                        model: config.model.clone(),
                                        max_tokens: 8192,
                                        system_prompt: String::new(),
                                        temperature: None,
                                    };

                                    streaming.is_streaming = true;
                                    streaming.phase = crate::domain::models::StreamingPhase::Idle;

                                    let handle = tokio::spawn(turn::run_turn(
                                        provider.clone(),
                                        messages,
                                        options,
                                        domain_tx.clone(),
                                    ));
                                    _active_turn = Some(handle);

                                    state.status_message = "Streaming...".to_string();
                                    state.needs_redraw = true;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Terminal event stream error: {}", e);
                        break;
                    }
                }
            }

            // Branch 2: Domain events (mpsc unbounded channel)
            Some(event) = domain_events_rx.recv() => {
                match event {
                    AppEvent::Shutdown => break,
                    AppEvent::ProviderChunk(chunk) => {
                        let action = apply_chunk(
                            &mut conversation,
                            &mut streaming,
                            chunk,
                            now_unix(),
                        );
                        match action {
                            ChunkAction::NeedsRedraw => {
                                state.needs_redraw = true;
                            }
                            ChunkAction::TurnComplete { .. } => {
                                state.status_message = "Ready".to_string();
                                state.needs_redraw = true;
                                _active_turn = None;
                            }
                            ChunkAction::TurnContinuing => {
                                // Sprint 0 safety: tool loop not yet implemented.
                                // Reset streaming state so the user isn't locked out.
                                // Sprint 1 (Story 1.5) replaces this with actual tool execution.
                                tracing::warn!("TurnContinuing received but tool loop not implemented — resetting streaming state");
                                streaming.is_streaming = false;
                                streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                _active_turn = None;
                                state.status_message = "Ready (tool_use unsupported in Sprint 0)".to_string();
                                state.needs_redraw = true;
                            }
                            ChunkAction::None => {}
                        }
                    }
                    AppEvent::SystemNotice(_level, msg) => {
                        state.status_message = msg;
                        state.needs_redraw = true;
                        streaming.is_streaming = false;
                    }
                    _ => {
                        state.needs_redraw = true;
                    }
                }
            }

            // Branch 3: Render tick (250ms interval with needs_redraw optimization)
            _ = tick_interval.tick() => {
                if state.needs_redraw {
                    render(terminal, &state, &config.model)?;
                    state.needs_redraw = false;
                }
            }

            // Branch 4: Active task monitoring (placeholder for future stories)
            // Currently a no-op future that never resolves
        }

        if state.should_quit {
            break;
        }
    }

    Ok(())
}

/// Check if the user is about to submit a message (Enter key in Input focus with text).
/// Returns the text if so. Must be called BEFORE handle_input clears the buffer.
fn check_message_submit(
    state: &TuiState,
    event: &crate::domain::events::DomainInputEvent,
) -> Option<String> {
    use crate::domain::events::{DomainInputEvent, DomainKey};
    use crate::domain::models::FocusState;

    if let DomainInputEvent::SpecialKey(DomainKey::Enter) = event {
        if matches!(state.focus, FocusState::Input) && !state.input_buffer.is_empty() {
            return Some(state.input_buffer.clone());
        }
    }
    None
}

/// Get current unix timestamp in seconds.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn is_ctrl_c(event: &crossterm::event::Event) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

/// Render the full TUI frame.
fn render(terminal: &mut Tui, state: &TuiState, model: &str) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        match layout::compute_layout(area, &state.theme) {
            Some(app_layout) => {
                let is_compact = area.width < 80 || area.height < 24;

                chat_pane::render(frame, app_layout.chat_pane, false, &state.theme);
                status_bar::render(
                    frame,
                    app_layout.status_bar,
                    model,
                    &state.status_message,
                    is_compact,
                    &state.theme,
                );
                input_box::render(
                    frame,
                    app_layout.input_area,
                    &state.input_buffer,
                    state.cursor_position,
                    state.focus,
                    &state.theme,
                );
            }
            None => {
                // Terminal too small
                let msg = ratatui::widgets::Paragraph::new("Terminal too small (min 60x16)")
                    .style(ratatui::prelude::Style::default().fg(ratatui::prelude::Color::Red))
                    .alignment(ratatui::prelude::Alignment::Center);
                frame.render_widget(msg, area);
            }
        }
    })?;

    Ok(())
}
