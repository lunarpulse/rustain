use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::adapters::tui::app::{InputAction, convert_crossterm_event, handle_input};
use crate::adapters::tui::color_detect::detect_color_capability;
use crate::adapters::tui::layout;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::terminal::Tui;
use crate::adapters::tui::widgets::{chat_pane, input_box, reverse_search, status_bar};

/// Timeout for background tasks (title generation, session save).
/// Separate from shutdown persist timeout (2s) which is more critical.
const BACKGROUND_TASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
use crate::domain::events::{AppEvent, ChunkAction};
use crate::domain::models::visual::{ConfirmationType, OverlayType};
use crate::domain::models::{
    AppConfig, ApprovalDecision, ChatMessage, CompletionOptions, Conversation, FeedbackAction,
    FeedbackBlock, FeedbackLevel, FocusState, MessageRole, RetryState, SessionManager, SessionState,
    StatusState, StreamChunk, StreamingState, UserMessage, apply_chunk, generate_conversation_id,
    next_delay,
};
use crate::domain::ports::{PersonaPort, ProviderPort, SecurityPort, StoragePort, ToolSetPort};
use crate::domain::services::message_builder;
use crate::domain::services::turn_queue::TurnQueue;
use crate::infrastructure::runtime::turn;

/// Run the 4-branch tokio::select! event loop.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    terminal: &mut Tui,
    domain_events_rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    config: &AppConfig,
    provider: Arc<dyn ProviderPort>,
    security: Arc<dyn SecurityPort>,
    tools: Arc<dyn ToolSetPort>,
    persona: Arc<dyn PersonaPort>,
    storage: Arc<dyn StoragePort>,
    fs_storage: Arc<crate::adapters::filesystem::FileSystemStorage>,
    workspace_path: std::path::PathBuf,
    restored_conversation: Option<Conversation>,
    recovery_prompt: Option<(String, u32)>,
) -> Result<()> {
    let size = terminal.size()?;
    let capability = detect_color_capability();
    let mut state = TuiState::with_capability(size.width, size.height, capability);

    // Set project context indicator based on persona
    state.has_project_context = !persona.system_prompt(&workspace_path).is_empty();

    let mut terminal_events = EventStream::new();
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(
        state.theme.timing.tick_interval_ms,
    ));

    // Conversation and streaming state for MVP single-tab
    let mut conversation = restored_conversation.unwrap_or_else(|| Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: Vec::new(),
        created_at: now_unix(),
        updated_at: now_unix(),
        last_response_at: None,
        session_id: None,
        usage: None,
        fork_source: None,
    });
    // Ensure session_id is set (new conversations don't have one yet)
    if conversation.session_id.is_none() {
        conversation.session_id = Some(generate_conversation_id());
    }

    let mut streaming = StreamingState::default();
    let mut turn_queue = TurnQueue::default();
    let mut _pending_save: Option<tokio::task::JoinHandle<()>> = None;

    // Initialize SessionManager based on whether we have a restored session
    let mut session_manager = if conversation.session_id.is_some() {
        SessionManager::new(SessionState::Active {
            id: conversation
                .session_id
                .clone()
                .unwrap_or_default(),
        })
    } else {
        SessionManager::new(SessionState::Empty)
    };

    // Mark session as in-flight (clean_exit = false) for crash detection
    if !conversation.messages.is_empty() {
        let fs_ref = fs_storage.clone();
        let conv_clone = conversation.clone();
        if let Err(e) = fs_ref.save_conversation_with_exit(&conv_clone, false).await {
            tracing::warn!("Failed to mark session in-flight: {}", e);
        }
    }

    // Track active turn task for cancellation
    let mut _active_turn: Option<tokio::task::JoinHandle<()>> = None;

    // Send recovery prompt if crash detected (before first render)
    if let Some((title, token_count)) = recovery_prompt {
        let _ = domain_tx.send(AppEvent::RecoveryPrompt { title, token_count });
    }

    // Render first frame immediately
    match render(
        terminal,
        &mut state,
        &conversation,
        &streaming,
        &config.model,
        security.as_ref(),
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
                            // Intercept input when recovery prompt is active
                            if state.active_feedback_id.as_deref() == Some("recovery") {
                                use crate::domain::events::DomainInputEvent;
                                match &domain_event {
                                    DomainInputEvent::SpecialKey(crate::domain::events::DomainKey::Enter)
                                    | DomainInputEvent::KeyPress('y') => {
                                        // Continue: dismiss recovery block, keep conversation
                                        state.feedback_blocks.remove("recovery");
                                        state.active_feedback_id = None;
                                        state.focus = FocusState::Input;
                                        state.needs_redraw = true;
                                        continue;
                                    }
                                    DomainInputEvent::SpecialKey(crate::domain::events::DomainKey::CtrlC)
                                    | DomainInputEvent::SpecialKey(crate::domain::events::DomainKey::Esc) => {
                                        // Always allow quit escape hatch
                                        state.feedback_blocks.remove("recovery");
                                        state.active_feedback_id = None;
                                        state.should_quit = true;
                                        continue;
                                    }
                                    DomainInputEvent::KeyPress('n') => {
                                        // New session: dismiss block, reset conversation
                                        state.feedback_blocks.remove("recovery");
                                        state.active_feedback_id = None;
                                        conversation.messages.clear();
                                        conversation.title = String::new();
                                        conversation.id = generate_conversation_id();
                                        conversation.session_id = Some(generate_conversation_id());
                                        conversation.created_at = now_unix();
                                        conversation.updated_at = now_unix();
                                        conversation.last_response_at = None;
                                        conversation.usage = None;
                                        state.focus = FocusState::Input;
                                        state.needs_redraw = true;
                                        // Persist the fresh conversation with clean_exit = true
                                        // (avoids phantom crash detection on next startup)
                                        let fs_ref = fs_storage.clone();
                                        let conv_clone = conversation.clone();
                                        if let Some(prev) = _pending_save.take() {
                                            let _ = prev.await;
                                        }
                                        _pending_save = Some(tokio::spawn(async move {
                                            match tokio::time::timeout(
                                                BACKGROUND_TASK_TIMEOUT,
                                                fs_ref.save_conversation_with_exit(&conv_clone, true),
                                            ).await {
                                                Ok(Ok(())) => {}
                                                Ok(Err(e)) => tracing::error!("Failed to persist new session: {}", e),
                                                Err(_) => tracing::warn!("Background task 'new_session_save' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs()),
                                            }
                                        }));
                                        continue;
                                    }
                                    _ => {
                                        // Block all other input while recovery prompt is active
                                        continue;
                                    }
                                }
                            }

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
                                            state.status_before_flash = Some(state.status.clone());
                                            state.status = StatusState::Flash {
                                            message: "Message queue full".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
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
                                            &security,
                                            &tools,
                                            &persona,
                                            &workspace_path,
                                            &mut session_manager,
                                        );
                                        // Force immediate render for typing indicator
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref()) {
                                            Ok(()) => state.needs_redraw = false,
                                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                        }
                                    }
                                }
                                InputAction::Quit => {
                                    state.should_quit = true;
                                }
                                InputAction::CancelOrQuit => {
                                    // Explicitly deny any pending permission before aborting
                                    if let Some(pending) = state.pending_permission.take() {
                                        let _ = pending.response_tx.send(ApprovalDecision::Deny);
                                        state.focus = FocusState::Input;
                                    }
                                    // Deny all queued permission requests
                                    while let Some(queued) = state.permission_queue.pop() {
                                        let _ = queued.response_tx.send(ApprovalDecision::Deny);
                                    }
                                    if streaming.is_streaming {
                                        // AC12: Finalize active tool calls with [aborted] before clearing
                                        for (_, tc) in streaming.active_tool_calls.iter_mut() {
                                            if tc.result.is_none() {
                                                tc.result = Some(crate::domain::models::ToolResultInfo {
                                                    content: "[aborted]".to_string(),
                                                    is_error: true,
                                                });
                                                tc.completed_at_ms = Some(now_unix() as u64 * 1000);
                                            }
                                        }

                                        // Abort streaming: preserve partial response
                                        if !streaming.current_text_buffer.is_empty()
                                            || !streaming.active_tool_calls.is_empty()
                                        {
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
                                        state.status = StatusState::Idle;
                                        state.needs_redraw = true;
                                    } else {
                                        // Not streaming → quit
                                        state.should_quit = true;
                                    }
                                }
                                InputAction::PermissionAllow => {
                                    if let Some(pending) = state.pending_permission.take() {
                                        let _ = pending.response_tx.send(ApprovalDecision::Allow);
                                        advance_permission_queue(&mut state);
                                    }
                                }
                                InputAction::PermissionDeny => {
                                    if let Some(pending) = state.pending_permission.take() {
                                        let _ = pending.response_tx.send(ApprovalDecision::Deny);
                                        advance_permission_queue(&mut state);
                                    }
                                }
                                InputAction::PermissionAlwaysAllow => {
                                    if let Some(pending) = state.pending_permission.take() {
                                        let _ = pending.response_tx.send(ApprovalDecision::AlwaysAllow);
                                        advance_permission_queue(&mut state);
                                    }
                                }
                                InputAction::FeedbackRetry => {
                                    // Clear the feedback block and retry the last user message
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    // Get retry attempt count
                                    let attempt = state.retry_state.as_ref().map_or(0, |r| r.attempt);
                                    let max_attempts: u8 = 5;
                                    if attempt >= max_attempts {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "Max retries reached".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                        state.retry_state = None;
                                        state.needs_redraw = true;
                                    } else {
                                        let delay = next_delay(attempt);
                                        state.retry_state = Some(RetryState {
                                            attempt: attempt + 1,
                                            max_attempts,
                                            delay_ms: delay,
                                        });
                                        state.status = StatusState::Retrying {
                                            attempt: attempt + 1,
                                            max: max_attempts,
                                            next_in_ms: delay,
                                        };
                                        state.needs_redraw = true;

                                        // Find the last user message to retry
                                        let last_user_text = conversation
                                            .messages
                                            .iter()
                                            .rev()
                                            .find(|m| m.role == MessageRole::User)
                                            .map(|m| m.content.clone());

                                        if let Some(text) = last_user_text {
                                            // Remove failed assistant response after the last user message
                                            if let Some(pos) = conversation.messages.iter().rposition(|m| m.role == MessageRole::User) {
                                                conversation.messages.truncate(pos);
                                            }
                                            // Schedule retry after actual backoff delay
                                            let delay_duration = std::time::Duration::from_millis(delay);
                                            let tx = domain_tx.clone();
                                            let retry_text = text;
                                            tokio::spawn(async move {
                                                tokio::time::sleep(delay_duration).await;
                                                let _ = tx.send(AppEvent::RetryMessage(retry_text));
                                            });
                                        }
                                    }
                                }
                                InputAction::SubmitQuestionAnswer(answer) => {
                                    // Send answer back via oneshot channel
                                    if let Some(tx) = state.question_response_tx.take() {
                                        let _ = tx.send(answer);
                                    }
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
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
                            ChunkAction::TurnComplete { persist, trigger_title_generation } => {
                                state.status = StatusState::Idle;
                                // Sync token usage from conversation to TUI state
                                state.token_usage = conversation.usage.clone();
                                // Clear stale feedback/retry state on successful turn
                                state.active_feedback_id = None;
                                state.retry_state = None;
                                state.needs_redraw = true;
                                _active_turn = None;

                                // Persist conversation on turn complete
                                if persist {
                                    let now = now_unix();
                                    conversation.updated_at = now;
                                    conversation.last_response_at = Some(now);
                                    // Await any previous in-flight save before starting a new one
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    let storage_ref = storage.clone();
                                    let conv_clone = conversation.clone();
                                    _pending_save = Some(tokio::spawn(async move {
                                        match tokio::time::timeout(
                                            BACKGROUND_TASK_TIMEOUT,
                                            storage_ref.save_conversation(&conv_clone),
                                        ).await {
                                            Ok(Ok(())) => {}
                                            Ok(Err(e)) => {
                                                tracing::error!("Failed to persist session: {}", e);
                                            }
                                            Err(_) => {
                                                tracing::warn!("Background task 'session_save' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs());
                                            }
                                        }
                                    }));
                                }

                                // Spawn background title generation (AC1, AC3, AC5)
                                if trigger_title_generation && conversation.title.is_empty()
                                    && conversation.messages.len() >= 2
                                {
                                    let provider_ref = provider.clone();
                                    let event_tx_ref = domain_tx.clone();
                                    let model = config.model.clone();
                                    let user_msg = conversation.messages[0].content.clone();
                                    let assistant_msg = truncate(&conversation.messages[1].content, 500);
                                    tokio::spawn(async move {
                                        match tokio::time::timeout(
                                            BACKGROUND_TASK_TIMEOUT,
                                            generate_title(&*provider_ref, &model, &user_msg, &assistant_msg),
                                        ).await {
                                            Ok(Ok(title)) => {
                                                let _ = event_tx_ref.send(AppEvent::TitleGenerated { title });
                                            }
                                            Ok(Err(e)) => {
                                                tracing::warn!("Title generation failed: {}", e);
                                            }
                                            Err(_) => {
                                                tracing::warn!("Background task 'title_generation' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs());
                                            }
                                        }
                                    });
                                }

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
                                        &security,
                                        &tools,
                                        &persona,
                                        &workspace_path,
                                        &mut session_manager,
                                    );
                                    // Force immediate render for typing indicator
                                    match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref()) {
                                        Ok(()) => state.needs_redraw = false,
                                        Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                    }
                                }
                            }
                            ChunkAction::TurnContinuing => {
                                // Tool execution loop is handled inside run_turn.
                                // The streaming stays active — tool results will arrive as
                                // ProviderChunk(ToolResult) followed by more streaming.
                                state.status = StatusState::Executing {
                                    tool_name: "tools".to_string(),
                                    elapsed_ms: 0,
                                };
                                state.needs_redraw = true;
                            }
                            ChunkAction::None => {}
                        }
                    }
                    AppEvent::SystemNotice(level, msg) => {
                        // Detect session expiry from provider authentication errors.
                        // Use tightened matches to avoid false positives (e.g., "401" in line numbers).
                        if matches!(level, crate::domain::models::NoticeLevel::Error)
                            && (msg.contains("Authentication failed")
                                || msg.contains("HTTP 401")
                                || msg.contains("status: 401")
                                || msg.to_lowercase().contains("session expired"))
                        {
                            session_manager.mark_invalidated(
                                conversation.session_id.clone(),
                            );
                            tracing::info!("Session invalidated due to auth error, will rebuild on next message");
                        }

                        // Only reset streaming state for Error/Warning notices.
                        // Info notices are informational and must NOT abort active streaming turns
                        // (e.g., session rebuild sends Info after spawning a new turn).
                        if !matches!(level, crate::domain::models::NoticeLevel::Info) {
                            streaming.is_streaming = false;
                            streaming.phase = crate::domain::models::StreamingPhase::Idle;
                            // Preserve partial response text (NFR21) — do NOT clear current_text_buffer
                            // Only clear blocks tracking and tool calls
                            streaming.current_blocks.clear();
                            streaming.active_tool_calls.clear();
                            // Abort the active turn task if running
                            if let Some(handle) = _active_turn.take() {
                                handle.abort();
                            }
                        }

                        // Create FeedbackBlock for errors and warnings; flash for info
                        match level {
                            crate::domain::models::NoticeLevel::Error => {
                                static FB_COUNTER: AtomicUsize = AtomicUsize::new(0);
                                let fb_id = format!("fb-{}", FB_COUNTER.fetch_add(1, Ordering::Relaxed));
                                let fb = FeedbackBlock {
                                    id: fb_id.clone(),
                                    level: FeedbackLevel::Error,
                                    message: msg,
                                    actions: vec![FeedbackAction::Retry],
                                };
                                state.feedback_blocks.insert(fb_id.clone(), fb);
                                state.active_feedback_id = Some(fb_id);
                                state.status = StatusState::Idle;
                                state.focus = FocusState::Chat; // Switch to chat so [r] works
                            }
                            crate::domain::models::NoticeLevel::Warning => {
                                static WFB_COUNTER: AtomicUsize = AtomicUsize::new(0);
                                let fb_id = format!("wfb-{}", WFB_COUNTER.fetch_add(1, Ordering::Relaxed));
                                let fb = FeedbackBlock {
                                    id: fb_id.clone(),
                                    level: FeedbackLevel::Warning,
                                    message: msg,
                                    actions: vec![FeedbackAction::Compact],
                                };
                                state.feedback_blocks.insert(fb_id, fb);
                                // Don't change focus — warning is informational, not blocking
                            }
                            _ => {
                                state.status_before_flash = Some(state.status.clone());
                                state.status = StatusState::Flash {
                                    message: msg,
                                    remaining_ms: state.theme.timing.status_flash_ms,
                                };
                            }
                        }
                        state.needs_redraw = true;
                    }
                    AppEvent::PermissionRequest {
                        tool_name,
                        tool_input,
                        response_tx,
                    } => {
                        use crate::adapters::tui::state::PendingPermission;
                        let new_pending = PendingPermission {
                            tool_name,
                            tool_input,
                            response_tx,
                        };
                        if state.pending_permission.is_some() {
                            // Queue if another permission prompt is already displayed
                            state.permission_queue.push(new_pending);
                        } else {
                            state.pending_permission = Some(new_pending);
                            state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                ConfirmationType::Permission,
                            ));
                        }
                        state.needs_redraw = true;
                    }
                    AppEvent::SetPermissionMode(mode) => {
                        security.set_mode(mode);
                        let mode_str = match mode {
                            crate::domain::models::PermissionMode::Normal => "Normal",
                            crate::domain::models::PermissionMode::Yolo => "YOLO",
                        };
                        state.status_before_flash = Some(state.status.clone());
                        state.status = StatusState::Flash {
                            message: format!("Permission mode: {}", mode_str),
                            remaining_ms: state.theme.timing.status_flash_ms,
                        };
                        state.needs_redraw = true;
                    }
                    AppEvent::RetryMessage(text) => {
                        // Delayed retry arrived — start the turn now
                        state.status = StatusState::Streaming;
                        start_turn(
                            &text,
                            &mut conversation,
                            &mut streaming,
                            &mut state,
                            &mut _active_turn,
                            &provider,
                            config,
                            &domain_tx,
                            &security,
                            &tools,
                            &persona,
                            &workspace_path,
                            &mut session_manager,
                        );
                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref()) {
                            Ok(()) => state.needs_redraw = false,
                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                        }
                    }
                    AppEvent::TitleGenerated { title } => {
                        conversation.title = title;
                        state.needs_redraw = true;
                        // Persist the updated title
                        if let Some(prev) = _pending_save.take() {
                            let _ = prev.await;
                        }
                        let storage_ref = storage.clone();
                        let conv_clone = conversation.clone();
                        _pending_save = Some(tokio::spawn(async move {
                            match tokio::time::timeout(
                                BACKGROUND_TASK_TIMEOUT,
                                storage_ref.save_conversation(&conv_clone),
                            ).await {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => {
                                    tracing::error!("Failed to persist title: {}", e);
                                }
                                Err(_) => {
                                    tracing::warn!("Background task 'title_save' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs());
                                }
                            }
                        }));
                    }
                    AppEvent::AskUserQuestion {
                        tool_use_id,
                        question,
                        response_tx,
                    } => {
                        use crate::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
                        state.ask_user_question = Some(AskUserQuestionState {
                            tool_use_id,
                            question,
                            input_buffer: String::new(),
                            cursor_position: 0,
                            submitted_answer: None,
                        });
                        state.question_response_tx = Some(response_tx);
                        state.focus = FocusState::Overlay(OverlayType::Confirmation(
                            ConfirmationType::Question,
                        ));
                        state.needs_redraw = true;
                    }
                    AppEvent::RecoveryPrompt { title, token_count } => {
                        let msg = format!(
                            "\u{2139} Recovered: '{}' (partial response, {} tokens). [Enter/y] continue [n] new",
                            title, token_count,
                        );
                        let fb = FeedbackBlock {
                            id: "recovery".to_string(),
                            level: FeedbackLevel::Info,
                            message: msg,
                            actions: vec![
                                FeedbackAction::Custom("[Enter] continue".to_string()),
                                FeedbackAction::StartFresh,
                            ],
                        };
                        state.feedback_blocks.insert("recovery".to_string(), fb);
                        state.active_feedback_id = Some("recovery".to_string());
                        state.focus = FocusState::Chat;
                        state.needs_redraw = true;
                    }
                    _ => {
                        state.needs_redraw = true;
                    }
                }
            }

            // Branch 3: Render tick (250ms interval with needs_redraw optimization)
            _ = tick_interval.tick() => {
                // Update elapsed_ms for Executing state each tick
                let tick_ms = state.theme.timing.tick_interval_ms;
                if let StatusState::Executing { elapsed_ms, .. } = &mut state.status {
                    *elapsed_ms += tick_ms;
                    state.needs_redraw = true;
                }

                // Flash message expiry: decrement remaining_ms each tick
                if let StatusState::Flash { remaining_ms, .. } = &mut state.status {
                    if *remaining_ms <= tick_ms {
                        // Flash expired — revert to previous status or Idle
                        state.status = state.status_before_flash.take().unwrap_or(StatusState::Idle);
                        state.needs_redraw = true;
                    } else {
                        *remaining_ms -= tick_ms;
                    }
                }

                if state.needs_redraw {
                    match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref()) {
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

    // Await any in-flight save before shutdown persist
    if let Some(prev) = _pending_save.take() {
        let _ = prev.await;
    }

    // Persist conversation before shutdown with clean_exit = true (graceful shutdown)
    if !conversation.messages.is_empty() {
        conversation.updated_at = now_unix();
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fs_storage.save_conversation_with_exit(&conversation, true),
        )
        .await
        {
            Ok(Ok(())) => {
                tracing::info!("Session persisted on shutdown (clean_exit=true)");
            }
            Ok(Err(e)) => {
                tracing::error!("Failed to persist session on shutdown: {}", e);
            }
            Err(_) => {
                tracing::warn!("Session save timed out on shutdown (>2s), proceeding with teardown");
            }
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
    security: &Arc<dyn SecurityPort>,
    tools: &Arc<dyn ToolSetPort>,
    persona: &Arc<dyn PersonaPort>,
    workspace_path: &std::path::Path,
    session_manager: &mut SessionManager,
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
    let mut messages = message_builder::build_api_messages(conversation);

    // If session manager indicates history rebuild needed, prepend context
    if session_manager.needs_history_rebuild() {
        use crate::domain::services::history_rebuild;
        let context = history_rebuild::build_history_context(&conversation.messages[..conversation.messages.len().saturating_sub(1)]);
        // Attach context_prefix to the actual user input message (matching `text`),
        // not to a synthetic tool-result message that build_api_messages may have appended.
        if let Some(target_msg) = messages.iter_mut().rev().find(|m| m.role == MessageRole::User && m.content == text) {
            target_msg.context_prefix = Some(context);
        } else if let Some(last_msg) = messages.last_mut() {
            // Fallback: attach to last message if exact match not found
            last_msg.context_prefix = Some(context);
        }
        let new_session_id = generate_conversation_id();
        conversation.session_id = Some(new_session_id.clone());
        session_manager.mark_active(new_session_id);
        let msg_count = conversation.messages.len().saturating_sub(1);
        let _ = domain_tx.send(AppEvent::SystemNotice(
            crate::domain::models::NoticeLevel::Info,
            format!("\u{2139}\u{fe0f} Session restarted with your conversation history ({} messages).", msg_count),
        ));
    }

    let tool_defs = tools.available_tools();
    let options = CompletionOptions {
        model: config.model.clone(),
        max_tokens: 8192,
        system_prompt: persona.system_prompt(workspace_path),
        temperature: None,
        tools: tool_defs,
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
        security.clone(),
        tools.clone(),
    ));
    *active_turn = Some(handle);

    state.status = StatusState::Streaming;
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

    state.status_before_flash = Some(state.status.clone());
    state.status = StatusState::Flash {
        message: format!("Render failed: {}", err),
        remaining_ms: state.theme.timing.status_flash_ms,
    };
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

/// Advance the permission queue: pop next pending request or restore Input focus.
fn advance_permission_queue(state: &mut TuiState) {
    if let Some(next) = state.permission_queue.pop() {
        state.pending_permission = Some(next);
        state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission));
    } else {
        state.focus = FocusState::Input;
    }
    state.needs_redraw = true;
}

/// Render the full TUI frame.
fn render(
    terminal: &mut Tui,
    state: &mut TuiState,
    conversation: &Conversation,
    streaming: &StreamingState,
    model: &str,
    security: &dyn SecurityPort,
) -> Result<()> {
    let scroll_offset = state.scroll_offset;
    let auto_scroll = state.auto_scroll;
    let mut content_height = 0usize;
    let mut block_bounds = Vec::new();
    let mut msg_bounds = Vec::new();
    let mut focused_tool_id: Option<String> = None;

    let permission_mode = security.current_mode();

    // Split borrows: height_cache needs &mut, tool_block_states needs &
    // Both are fields of state, so we extract refs before the closure.
    let TuiState {
        ref mut height_cache,
        ref tool_block_states,
        ref feedback_blocks,
        ref pending_permission,
        ref ask_user_question,
        ref theme,
        ref status,
        ref token_usage,
        ref input_buffer,
        cursor_position,
        focus,
        has_project_context,
        multiline_mode,
        input_scroll_offset,
        ref reverse_search,
        ..
    } = *state;

    terminal.draw(|frame| {
        let area = frame.area();

        match layout::compute_layout(area, theme, input_buffer) {
            Some(app_layout) => {
                let _is_compact = area.width < 80 || area.height < 24;

                let result = chat_pane::render(
                    frame,
                    app_layout.chat_pane,
                    conversation,
                    streaming,
                    scroll_offset,
                    auto_scroll,
                    theme,
                    height_cache,
                    tool_block_states,
                    feedback_blocks,
                );
                content_height = result.total_content_height;
                block_bounds = result.block_boundaries;
                msg_bounds = result.message_boundaries;
                focused_tool_id = result.focused_tool_id;

                // Render peek overlay if any tool block has peek_active (AC5)
                for (tool_id, tbs) in tool_block_states.iter() {
                    if tbs.peek_active {
                        // Find the tool call info for this tool block
                        let tc = conversation
                            .messages
                            .iter()
                            .flat_map(|m| m.tool_calls.iter())
                            .chain(streaming.active_tool_calls.values())
                            .find(|tc| tc.id == *tool_id);
                        if let Some(tc) = tc {
                            let (paragraph, area) =
                                crate::adapters::tui::widgets::tool_block::render_peek_overlay(
                                    tc,
                                    theme,
                                    app_layout.chat_pane,
                                );
                            frame.render_widget(ratatui::widgets::Clear, area);
                            frame.render_widget(paragraph, area);
                        }
                        break; // Only one peek at a time
                    }
                }

                // Render permission prompt at bottom of chat pane if pending
                if let Some(pending) = pending_permission {
                    use crate::adapters::tui::widgets::permission_prompt;
                    let prompt_lines = permission_prompt::render_permission_lines(
                        &pending.tool_name,
                        &pending.tool_input,
                        theme,
                    );
                    let prompt_height = prompt_lines.len() as u16;
                    let prompt_area = ratatui::prelude::Rect {
                        x: app_layout.chat_pane.x,
                        y: app_layout.chat_pane.y + app_layout.chat_pane.height
                            - prompt_height.min(app_layout.chat_pane.height),
                        width: app_layout.chat_pane.width,
                        height: prompt_height.min(app_layout.chat_pane.height),
                    };
                    let paragraph = ratatui::widgets::Paragraph::new(prompt_lines);
                    frame.render_widget(ratatui::widgets::Clear, prompt_area);
                    frame.render_widget(paragraph, prompt_area);
                }

                // Render AskUserQuestion card at bottom of chat pane if active
                if let Some(aq) = ask_user_question {
                    use crate::adapters::tui::widgets::ask_user_question;
                    let aq_lines = ask_user_question::render_ask_user_lines(
                        aq,
                        app_layout.chat_pane.width,
                        theme,
                    );
                    let aq_height = aq_lines.len() as u16;
                    let aq_area = ratatui::prelude::Rect {
                        x: app_layout.chat_pane.x,
                        y: app_layout.chat_pane.y + app_layout.chat_pane.height
                            - aq_height.min(app_layout.chat_pane.height),
                        width: app_layout.chat_pane.width,
                        height: aq_height.min(app_layout.chat_pane.height),
                    };
                    let paragraph = ratatui::widgets::Paragraph::new(aq_lines);
                    frame.render_widget(ratatui::widgets::Clear, aq_area);
                    frame.render_widget(paragraph, aq_area);
                }

                let session_title = if !conversation.title.is_empty() {
                    Some(conversation.title.as_str())
                } else {
                    None
                };
                status_bar::render(
                    frame,
                    app_layout.status_bar,
                    model,
                    status,
                    theme,
                    scroll_offset,
                    &msg_bounds,
                    content_height,
                    app_layout.chat_pane.height,
                    permission_mode,
                    token_usage.as_ref(),
                    has_project_context,
                    session_title,
                    multiline_mode,
                );
                input_box::render(
                    frame,
                    app_layout.input_area,
                    input_buffer,
                    cursor_position,
                    focus,
                    theme,
                    multiline_mode,
                    input_scroll_offset,
                );

                // Render reverse search overlay above input box
                if reverse_search.active {
                    reverse_search::render(
                        frame,
                        app_layout.input_area,
                        reverse_search,
                        theme,
                    );
                }
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
    state.focused_tool_id = focused_tool_id;

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

/// Truncate a string to `max_len` characters, appending "…" if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max_len).collect();
        truncated.push('…');
        truncated
    }
}

/// Generate a concise title for a conversation using the LLM provider.
async fn generate_title(
    provider: &dyn ProviderPort,
    model: &str,
    user_msg: &str,
    assistant_msg: &str,
) -> Result<String> {
    use crate::domain::models::{CompletionOptions, Message, MessageRole as MsgRole};

    let prompt_content = format!(
        "User: {}\n\nAssistant: {}",
        user_msg, assistant_msg
    );
    let messages = vec![Message {
        role: MsgRole::User,
        content: prompt_content,
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
    }];
    let options = CompletionOptions {
        model: model.to_string(),
        max_tokens: 30,
        system_prompt: "Generate a concise title (under 60 characters) for this conversation. Output only the title, no quotes or explanation.".to_string(),
        temperature: None,
        tools: vec![],
    };

    let stream = provider.stream_completion(messages, options).await?;
    futures::pin_mut!(stream);

    let mut title = String::new();
    while let Some(chunk) = stream.next().await {
        if let StreamChunk::Text { content, .. } = chunk {
            title.push_str(&content);
        }
    }

    // Post-process: trim, strip surrounding quotes, enforce 60-char max
    let title = post_process_title(&title);
    if title.is_empty() {
        anyhow::bail!("Title generation produced empty result");
    }
    Ok(title)
}

/// Post-process a generated title: trim whitespace, strip surrounding quotes,
/// enforce 60-character maximum with ellipsis truncation.
pub fn post_process_title(raw: &str) -> String {
    let mut title = raw.trim().to_string();
    // Strip surrounding quotes (both " and ') — uses strip_prefix/strip_suffix to avoid
    // byte-index panics on single-char or multi-byte inputs (review finding F3)
    if let Some(inner) = title.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        title = inner.to_string();
    } else if let Some(inner) = title.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        title = inner.to_string();
    }
    // Enforce 60-char max
    if title.chars().count() > 60 {
        title = title.chars().take(57).collect::<String>() + "...";
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_post_process_title_trims_whitespace() {
        assert_eq!(post_process_title("  Hello World  "), "Hello World");
    }

    #[test]
    fn test_post_process_title_strips_double_quotes() {
        assert_eq!(post_process_title("\"Quoted Title\""), "Quoted Title");
    }

    #[test]
    fn test_post_process_title_strips_single_quotes() {
        assert_eq!(post_process_title("'Single Quoted'"), "Single Quoted");
    }

    #[test]
    fn test_post_process_title_no_strip_mismatched_quotes() {
        assert_eq!(post_process_title("\"Mismatched'"), "\"Mismatched'");
    }

    #[test]
    fn test_post_process_title_single_char_quote_no_panic() {
        // F3: previously panicked with title[1..0] on single-char quote
        assert_eq!(post_process_title("\""), "\"");
        assert_eq!(post_process_title("'"), "'");
    }

    #[test]
    fn test_post_process_title_empty_inside_quotes() {
        assert_eq!(post_process_title("\"\""), "");
        assert_eq!(post_process_title("''"), "");
    }

    #[test]
    fn test_post_process_title_truncates_at_60_chars() {
        let long = "A".repeat(70);
        let result = post_process_title(&long);
        assert_eq!(result.len(), 60); // 57 A's + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_post_process_title_preserves_under_60() {
        let short = "Short Title";
        assert_eq!(post_process_title(short), "Short Title");
    }

    #[test]
    fn test_post_process_title_exactly_60_chars() {
        let exact = "A".repeat(60);
        assert_eq!(post_process_title(&exact), exact);
    }

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("Hello", 10), "Hello");
    }

    #[test]
    fn test_truncate_long_string() {
        let long = "A".repeat(600);
        let result = truncate(&long, 500);
        assert_eq!(result.chars().count(), 501); // 500 chars + '…'
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_truncate_exact_length() {
        let exact = "A".repeat(500);
        assert_eq!(truncate(&exact, 500), exact);
    }
}
