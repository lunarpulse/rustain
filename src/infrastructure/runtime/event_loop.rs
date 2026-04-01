use std::sync::Arc;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::adapters::tui::app::{InputAction, convert_crossterm_event, handle_input};
use crate::adapters::tui::color_detect::detect_color_capability;
use crate::adapters::tui::layout;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::terminal::Tui;
use crate::adapters::tui::widgets::{chat_pane, input_box, status_bar};
use crate::domain::events::{AppEvent, ChunkAction};
use crate::domain::models::{
    AppConfig, ChatMessage, CompletionOptions, Conversation, MessageRole, StreamingState,
    UserMessage, apply_chunk, generate_conversation_id,
};
use crate::domain::ports::ProviderPort;
use crate::domain::services::message_builder;
use crate::domain::services::turn_queue::TurnQueue;
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
    let mut turn_queue = TurnQueue::default();

    // Track active turn task for cancellation
    let mut _active_turn: Option<tokio::task::JoinHandle<()>> = None;

    // Render first frame immediately
    render(
        terminal,
        &mut state,
        &conversation,
        &streaming,
        &config.model,
    )?;
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
                            let action = handle_input(&mut state, &domain_event);

                            match action {
                                InputAction::SubmitMessage(text) => {
                                    if streaming.is_streaming {
                                        // Queue message during streaming
                                        let msg = UserMessage {
                                            content: text,
                                            images: vec![],
                                        };
                                        if turn_queue.enqueue(msg).is_err() {
                                            state.status_message = "Message queue full".to_string();
                                            state.needs_redraw = true;
                                        }
                                    } else {
                                        start_turn(
                                            &text,
                                            &mut conversation,
                                            &mut streaming,
                                            &mut state,
                                            &mut _active_turn,
                                            &provider,
                                            config,
                                            &domain_tx,
                                        );
                                        // Force immediate render for typing indicator
                                        render(terminal, &mut state, &conversation, &streaming, &config.model)?;
                                        state.needs_redraw = false;
                                    }
                                }
                                InputAction::Quit => {
                                    state.should_quit = true;
                                }
                                InputAction::Consumed | InputAction::Ignored => {}
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

                                // Auto-send queued messages
                                if let Some(queued_msg) = turn_queue.dequeue() {
                                    start_turn(
                                        &queued_msg.content,
                                        &mut conversation,
                                        &mut streaming,
                                        &mut state,
                                        &mut _active_turn,
                                        &provider,
                                        config,
                                        &domain_tx,
                                    );
                                    // Force immediate render for typing indicator
                                    render(terminal, &mut state, &conversation, &streaming, &config.model)?;
                                    state.needs_redraw = false;
                                }
                            }
                            ChunkAction::TurnContinuing => {
                                // Sprint 0 safety: tool loop not yet implemented.
                                // Reset streaming state so the user isn't locked out.
                                // Sprint 1 (Story 1.5) replaces this with actual tool execution.
                                tracing::warn!("TurnContinuing received but tool loop not implemented — resetting streaming state");
                                streaming.is_streaming = false;
                                streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                streaming.current_text_buffer.clear();
                                streaming.current_blocks.clear();
                                streaming.active_tool_calls.clear();
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
                        streaming.phase = crate::domain::models::StreamingPhase::Idle;
                        streaming.current_text_buffer.clear();
                        streaming.current_blocks.clear();
                        streaming.active_tool_calls.clear();
                        // Abort the active turn task if running
                        if let Some(handle) = _active_turn.take() {
                            handle.abort();
                        }
                    }
                    _ => {
                        state.needs_redraw = true;
                    }
                }
            }

            // Branch 3: Render tick (250ms interval with needs_redraw optimization)
            _ = tick_interval.tick() => {
                if state.needs_redraw {
                    render(terminal, &mut state, &conversation, &streaming, &config.model)?;
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

/// Start a new turn: add user message, spawn provider streaming task.
#[allow(clippy::too_many_arguments)]
fn start_turn(
    text: &str,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    state: &mut TuiState,
    active_turn: &mut Option<tokio::task::JoinHandle<()>>,
    provider: &Arc<dyn ProviderPort>,
    config: &AppConfig,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    // Add user ChatMessage to conversation
    conversation.messages.push(ChatMessage {
        role: MessageRole::User,
        content: text.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: now_unix(),
        token_count: None,
    });

    // Build messages list for provider
    let messages = message_builder::build_api_messages(conversation);

    let options = CompletionOptions {
        model: config.model.clone(),
        max_tokens: 8192,
        system_prompt: String::new(),
        temperature: None,
    };

    // Clear any stale buffers from a previous turn (e.g. after TurnContinuing or SystemNotice)
    streaming.current_text_buffer.clear();
    streaming.current_blocks.clear();
    streaming.active_tool_calls.clear();
    streaming.is_streaming = true;
    streaming.phase = crate::domain::models::StreamingPhase::AccumulatingText;

    let handle = tokio::spawn(turn::run_turn(
        provider.clone(),
        messages,
        options,
        domain_tx.clone(),
    ));
    *active_turn = Some(handle);

    state.status_message = "Streaming...".to_string();
    state.needs_redraw = true;
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
fn render(
    terminal: &mut Tui,
    state: &mut TuiState,
    conversation: &Conversation,
    streaming: &StreamingState,
    model: &str,
) -> Result<()> {
    let scroll_offset = state.scroll_offset;
    let auto_scroll = state.auto_scroll;
    let mut content_height = 0usize;

    terminal.draw(|frame| {
        let area = frame.area();

        match layout::compute_layout(area, &state.theme) {
            Some(app_layout) => {
                let is_compact = area.width < 80 || area.height < 24;

                content_height = chat_pane::render(
                    frame,
                    app_layout.chat_pane,
                    conversation,
                    streaming,
                    scroll_offset,
                    auto_scroll,
                    &state.theme,
                );
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

    state.total_content_height = content_height;

    Ok(())
}
