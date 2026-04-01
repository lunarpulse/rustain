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
    AppConfig, ChatMessage, CompletionOptions, Conversation, FocusState, MessageRole,
    StreamingState, UserMessage, apply_chunk, generate_conversation_id,
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
    match render(
        terminal,
        &mut state,
        &conversation,
        &streaming,
        &config.model,
    ) {
        Ok(()) => state.needs_redraw = false,
        Err(e) => {
            handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal);
            if state.should_quit {
                return Ok(());
            }
        }
    }

    loop {
        tokio::select! {
            // Branch 1: Terminal input (crossterm event stream)
            Some(event_result) = terminal_events.next() => {
                match event_result {
                    Ok(event) => {
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
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model) {
                                            Ok(()) => state.needs_redraw = false,
                                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                        }
                                    }
                                }
                                InputAction::Quit => {
                                    state.should_quit = true;
                                }
                                InputAction::CancelOrQuit => {
                                    if streaming.is_streaming {
                                        // Abort streaming: preserve partial response
                                        if !streaming.current_text_buffer.is_empty() {
                                            let content = std::mem::take(&mut streaming.current_text_buffer);
                                            conversation.messages.push(ChatMessage {
                                                role: MessageRole::Assistant,
                                                content,
                                                content_blocks: std::mem::take(&mut streaming.current_blocks),
                                                tool_calls: streaming.active_tool_calls.drain().map(|(_, v)| v).collect(),
                                                created_at: now_unix(),
                                                token_count: None,
                                                stop_reason: Some(crate::domain::models::StopReason::Cancelled),
                                            });
                                        }
                                        // Abort the active turn task
                                        if let Some(handle) = _active_turn.take() {
                                            handle.abort();
                                        }
                                        // Reset streaming state
                                        streaming.is_streaming = false;
                                        streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                        streaming.current_blocks.clear();
                                        streaming.active_tool_calls.clear();
                                        // Clear TurnQueue entirely
                                        while turn_queue.dequeue().is_some() {}
                                        // Ready for next input
                                        state.focus = FocusState::Input;
                                        state.status_message = "Ready".to_string();
                                        state.needs_redraw = true;
                                    } else {
                                        // Not streaming → quit
                                        state.should_quit = true;
                                    }
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
                                    match render(terminal, &mut state, &conversation, &streaming, &config.model) {
                                        Ok(()) => state.needs_redraw = false,
                                        Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                    }
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
                    match render(terminal, &mut state, &conversation, &streaming, &config.model) {
                        Ok(()) => state.needs_redraw = false,
                        Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                    }
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
        stop_reason: None,
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

/// Handle a render failure: abort active turn, reset streaming, attempt terminal recovery.
fn handle_render_error(
    err: anyhow::Error,
    active_turn: &mut Option<tokio::task::JoinHandle<()>>,
    streaming: &mut StreamingState,
    state: &mut TuiState,
    terminal: &mut Tui,
) {
    tracing::error!("Render failed: {}", err);

    // Abort active turn if running
    if let Some(handle) = active_turn.take() {
        handle.abort();
    }

    // Reset streaming state
    streaming.is_streaming = false;
    streaming.phase = crate::domain::models::StreamingPhase::Idle;
    streaming.current_text_buffer.clear();
    streaming.current_blocks.clear();
    streaming.active_tool_calls.clear();

    state.status_message = format!("Render failed: {}", err);
    state.needs_redraw = true;

    // Attempt terminal recovery
    crate::adapters::tui::terminal::restore_terminal_raw();
    match crate::adapters::tui::terminal::setup() {
        Ok(new_terminal) => {
            *terminal = new_terminal;
            tracing::info!("Terminal recovered after render failure");
        }
        Err(recovery_err) => {
            tracing::error!("Terminal recovery failed: {}", recovery_err);
            state.should_quit = true;
        }
    }
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
    let mut block_bounds = Vec::new();
    let mut msg_bounds = Vec::new();
    let height_cache = &mut state.height_cache;

    terminal.draw(|frame| {
        let area = frame.area();

        match layout::compute_layout(area, &state.theme) {
            Some(app_layout) => {
                let is_compact = area.width < 80 || area.height < 24;

                let result = chat_pane::render(
                    frame,
                    app_layout.chat_pane,
                    conversation,
                    streaming,
                    scroll_offset,
                    auto_scroll,
                    &state.theme,
                    height_cache,
                );
                content_height = result.total_content_height;
                block_bounds = result.block_boundaries;
                msg_bounds = result.message_boundaries;
                status_bar::render(
                    frame,
                    app_layout.status_bar,
                    model,
                    &state.status_message,
                    is_compact,
                    &state.theme,
                    scroll_offset,
                    &msg_bounds,
                    content_height,
                    app_layout.chat_pane.height,
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
    state.block_boundaries = block_bounds;
    state.message_boundaries = msg_bounds;

    // Resolve pending anchor from resize: use new heights to find correct scroll_offset.
    if let Some(anchor_idx) = state.pending_anchor.take() {
        if anchor_idx < state.block_boundaries.len() {
            let anchor_line = state.block_boundaries[anchor_idx];
            let vp = state.terminal_height as usize;
            let max_offset = content_height.saturating_sub(vp);
            state.scroll_offset = max_offset.saturating_sub(anchor_line);
            state.auto_scroll = state.scroll_offset == 0;
        } else {
            // Anchor no longer valid (conversation changed during resize) — fall back to bottom
            state.scroll_offset = 0;
            state.auto_scroll = true;
        }
    }

    Ok(())
}
