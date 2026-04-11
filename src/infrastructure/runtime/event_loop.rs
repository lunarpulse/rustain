use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::adapters::command_registry::CommandRegistry;
use crate::adapters::file_scanner;
use crate::adapters::palette_registry::PaletteRegistry;
use crate::adapters::tui::app::{InputAction, convert_crossterm_event, handle_input};
use crate::adapters::tui::color_detect::detect_color_capability;
use crate::adapters::tui::layout;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::terminal::Tui;
use crate::adapters::tui::widgets::{
    autocomplete_popup, chat_pane, command_palette as command_palette_widget, help_overlay,
    input_box, reverse_search, status_bar, which_key_bar,
};

/// Timeout for background tasks (title generation, session save).
/// Separate from shutdown persist timeout (2s) which is more critical.
const BACKGROUND_TASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
use crate::domain::events::{AppEvent, ChunkAction};
use crate::domain::models::tab::TabManager;
use crate::domain::models::visual::{ConfirmationType, OverlayType};
use crate::domain::models::{
    AppConfig, ApprovalDecision, ChatMessage, CompletionOptions, Conversation, FeedbackAction,
    FeedbackBlock, FeedbackLevel, FocusState, ImageAttachment, MessageRole, RetryState,
    SessionManager, SessionState, StatusState, StreamChunk, StreamingState, UserMessage,
    apply_chunk, generate_conversation_id, next_delay,
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

    // Cache multiplexer detection once at startup (UX-DR62)
    state.multiplexer_detected = crate::adapters::tui::help_data::is_multiplexer_session();

    // Cache VS Code terminal detection once at startup (Sprint Change Proposal 2026-04-08, AC#4, AC9)
    state.is_vscode = crate::infrastructure::utils::is_vscode_terminal();

    // Load and increment session count for contextual hint fading (UX-DR96)
    state.session_count = crate::adapters::tui::hints::load_and_increment_session_count();
    // Compute initial hint
    state.current_hint = crate::adapters::tui::hints::contextual_hint(
        &state.focus,
        state.session_count,
        state.theme.timing.status_hint_fade_sessions,
        state.is_vscode,
    );

    // Set project context indicator based on persona
    state.has_project_context = !persona.system_prompt(&workspace_path).is_empty();

    let mut terminal_events = EventStream::new();
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(
        state.theme.timing.tick_interval_ms,
    ));

    // Tab manager — owns all per-tab state; standalone proxies stay in sync with the active tab
    let mut tab_manager = if let Some(conv) = restored_conversation {
        TabManager::with_conversation(conv)
    } else {
        TabManager::new()
    };
    // Ensure session_id is set on the active tab's conversation
    if tab_manager.active_tab().conversation.session_id.is_none() {
        let sid = generate_conversation_id();
        tab_manager.active_tab_mut().conversation.session_id = Some(sid);
    }

    // Active-tab proxies — always reflect the current tab; synced on every tab switch
    let mut conversation = tab_manager.active_tab().conversation.clone();
    let mut streaming = tab_manager.active_tab().streaming.clone();
    let mut turn_queue = TurnQueue::default();
    let mut _pending_save: Option<tokio::task::JoinHandle<()>> = None;

    // Lazy-initialized command registry (NFR10: not scanned at startup)
    let mut command_registry = CommandRegistry::new();

    // Lazy-initialized palette registry (populated on first Ctrl+P)
    let mut palette_registry = PaletteRegistry::new();

    // P4: Cache last filter text to avoid re-scanning on every keystroke
    let mut last_autocomplete_filter = String::new();
    // P6: Cache last palette filter text to avoid re-filtering on every keystroke
    let mut last_palette_filter = String::new();

    // Initialize SessionManager from the active tab
    let mut session_manager = tab_manager.active_tab().session.clone();

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
        let _ = domain_tx.send(AppEvent::RecoveryPrompt {
            conversation_id: conversation.id.clone(),
            title,
            token_count,
        });
    }

    // Render first frame immediately
    match render(
        terminal,
        &mut state,
        &conversation,
        &streaming,
        &config.model,
        security.as_ref(),
        tab_manager.tab_count(),
        tab_manager.active_tab_index(),
        Some(&tab_manager),
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

                            // Update contextual hint whenever focus may have changed (UX-DR93)
                            if state.needs_redraw {
                                state.current_hint = crate::adapters::tui::hints::contextual_hint(
                                    &state.focus,
                                    state.session_count,
                                    state.theme.timing.status_hint_fade_sessions,
                                    state.is_vscode,
                                );
                            }

                            // P4: Only re-populate autocomplete when filter text actually changed
                            if state.autocomplete.active && state.autocomplete.filter_text != last_autocomplete_filter {
                                last_autocomplete_filter = state.autocomplete.filter_text.clone();
                                populate_autocomplete_suggestions(
                                    &mut state,
                                    &mut command_registry,
                                    &workspace_path,
                                );
                            } else if !state.autocomplete.active {
                                last_autocomplete_filter.clear();
                            }

                            // Repopulate command palette entries only when filter changes (P6)
                            if state.command_palette.active
                                && state.command_palette.filter_text != last_palette_filter
                            {
                                last_palette_filter = state.command_palette.filter_text.clone();
                                populate_palette_entries(
                                    &mut state,
                                    &mut command_registry,
                                    &mut palette_registry,
                                    &workspace_path,
                                );
                            } else if !state.command_palette.active {
                                last_palette_filter.clear();
                            }

                            match action {
                                InputAction::SubmitMessage(text) => {
                                    // Resolve any file mentions from autocomplete selections
                                    let mut mention_images: Vec<ImageAttachment> = Vec::new();
                                    let send_text = if !state.resolved_mentions.is_empty() {
                                        let (file_contexts, file_images, file_errors) = resolve_file_context(&state.resolved_mentions, &workspace_path, security.as_ref());
                                        mention_images = file_images;
                                        state.resolved_mentions.clear();
                                        // P9: Show FeedbackBlock for each file resolution error
                                        for err in &file_errors {
                                            let fb = FeedbackBlock {
                                                id: generate_conversation_id(),
                                                message: err.reason.clone(),
                                                level: FeedbackLevel::Error,
                                                actions: vec![],
                                            };
                                            state.feedback_blocks.insert(fb.id.clone(), fb);
                                            state.needs_redraw = true;
                                        }
                                        if !file_contexts.is_empty() {
                                            let prefix = message_builder::build_file_context_prefix(&file_contexts);
                                            format!("{}{}", prefix, text)
                                        } else {
                                            text
                                        }
                                    } else {
                                        text
                                    };
                                    // Merge pending images (clipboard paste) with mention images (@ file references)
                                    // Covers: FR112 (AC1, AC2)
                                    let mut all_images: Vec<ImageAttachment> = state.pending_images.drain(..).collect();
                                    all_images.extend(mention_images);
                                    state.image_indicator = None;

                                    if streaming.is_streaming {
                                        // Queue message during streaming
                                        let msg = UserMessage {
                                            content: send_text,
                                            images: all_images,
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
                                            &send_text,
                                            all_images,
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
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager)) {
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
                                                id: generate_conversation_id(),
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
                                            let retry_conv_id = conversation.id.clone();
                                            tokio::spawn(async move {
                                                tokio::time::sleep(delay_duration).await;
                                                let _ = tx.send(AppEvent::RetryMessage {
                                                    conversation_id: retry_conv_id,
                                                    content: retry_text,
                                                });
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
                                InputAction::ExecuteCommand(cmd) => {
                                    if cmd == "ml" {
                                        // /ml command: toggle multi-line mode (AC#3)
                                        // Covers: Sprint Change Proposal 2026-04-08
                                        state.multiline_mode = !state.multiline_mode;
                                        state.needs_redraw = true;
                                    } else if cmd == "new" {
                                        // /new command: save current, create fresh session
                                        // AC7: save current conversation if it has messages
                                        if !conversation.messages.is_empty() {
                                            conversation.updated_at = now_unix();
                                            if let Some(prev) = _pending_save.take() {
                                                let _ = prev.await;
                                            }
                                            let fs_ref = fs_storage.clone();
                                            let conv_clone = conversation.clone();
                                            _pending_save = Some(tokio::spawn(async move {
                                                match tokio::time::timeout(
                                                    BACKGROUND_TASK_TIMEOUT,
                                                    fs_ref.save_conversation_with_exit(&conv_clone, true),
                                                ).await {
                                                    Ok(Ok(())) => {}
                                                    Ok(Err(e)) => tracing::error!("Failed to save session before /new: {}", e),
                                                    Err(_) => tracing::warn!("Session save timed out before /new"),
                                                }
                                            }));
                                        }
                                        // Create fresh conversation
                                        conversation = Conversation {
                                            id: generate_conversation_id(),
                                            title: String::new(),
                                            messages: Vec::new(),
                                            created_at: now_unix(),
                                            updated_at: now_unix(),
                                            last_response_at: None,
                                            session_id: Some(generate_conversation_id()),
                                            usage: None,
                                            fork_source: None,
                                        };
                                        // Reset TUI state
                                        state.input_buffer.clear();
                                        state.cursor_position = 0;
                                        state.input_scroll_offset = 0;
                                        state.scroll_offset = 0;
                                        state.auto_scroll = true;
                                        state.total_content_height = 0;
                                        state.block_boundaries.clear();
                                        state.message_boundaries.clear();
                                        state.height_cache.invalidate_all();
                                        state.tool_block_states.clear();
                                        state.focused_tool_id = None;
                                        state.feedback_blocks.clear();
                                        state.active_feedback_id = None;
                                        state.autocomplete.dismiss();
                                        state.resolved_mentions.clear();
                                        state.token_usage = None;
                                        state.focus = FocusState::Input;
                                        state.status = StatusState::Idle;
                                        state.needs_redraw = true;
                                        // Reset session manager
                                        session_manager = SessionManager::new(SessionState::Active {
                                            id: conversation.session_id.clone().unwrap_or_default(),
                                        });
                                        // Reset streaming state
                                        streaming = StreamingState::default();
                                        while turn_queue.dequeue().is_some() {}
                                        if let Some(handle) = _active_turn.take() {
                                            handle.abort();
                                        }
                                    }
                                }
                                InputAction::SubmitWithContext { text, command } => {
                                    // Build context-enriched message
                                    let mut context_prefix = String::new();

                                    // Resolve command context if present
                                    if let Some(ref cmd_name) = command {
                                        if let Some(cmd_ctx) = resolve_command_context(cmd_name, &command_registry) {
                                            context_prefix.push_str(&message_builder::build_command_context_prefix(&cmd_ctx));
                                        }
                                    }

                                    // Resolve file mentions
                                    let (file_contexts, mention_images, file_errors) = resolve_file_context(&state.resolved_mentions, &workspace_path, security.as_ref());
                                    if !file_contexts.is_empty() {
                                        context_prefix.push_str(&message_builder::build_file_context_prefix(&file_contexts));
                                    }
                                    // P9: Show FeedbackBlock for each file resolution error
                                    for err in &file_errors {
                                        let fb = FeedbackBlock {
                                            id: generate_conversation_id(),
                                            message: err.reason.clone(),
                                            level: FeedbackLevel::Error,
                                            actions: vec![],
                                        };
                                        state.feedback_blocks.insert(fb.id.clone(), fb);
                                        state.needs_redraw = true;
                                    }
                                    state.resolved_mentions.clear();

                                    let full_text = if context_prefix.is_empty() {
                                        text
                                    } else {
                                        format!("{}{}", context_prefix, text)
                                    };

                                    // Merge pending images with mention images
                                    // Covers: FR112 (AC1, AC2)
                                    let mut all_images: Vec<ImageAttachment> = state.pending_images.drain(..).collect();
                                    all_images.extend(mention_images);
                                    state.image_indicator = None;

                                    if streaming.is_streaming {
                                        let msg = UserMessage {
                                            content: full_text,
                                            images: all_images,
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
                                            &full_text,
                                            all_images,
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
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager)) {
                                            Ok(()) => state.needs_redraw = false,
                                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                        }
                                    }
                                }
                                InputAction::CopyToClipboard(mut content) => {
                                    use crate::adapters::tui::clipboard;

                                    // Resolve content if empty (Chat focus copy)
                                    // Covers: FR116 (AC6, AC8, AC9)
                                    if content.is_empty() {
                                        content = resolve_copy_content(&state, &conversation);
                                    }
                                    if content.is_empty() {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "Nothing to copy".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                        state.needs_redraw = true;
                                    } else {

                                    let result = clipboard::copy_to_clipboard(&content);
                                    let flash_msg = match result {
                                        clipboard::ClipboardResult::Osc52Success => {
                                            "Copied to clipboard".to_string()
                                        }
                                        clipboard::ClipboardResult::FallbackSuccess(ref path) => {
                                            format!("Copied to {}", path.display())
                                        }
                                        clipboard::ClipboardResult::Failed(ref err) => {
                                            format!("Copy failed: {}", err)
                                        }
                                    };
                                    state.status_before_flash = Some(state.status.clone());
                                    state.status = StatusState::Flash {
                                        message: flash_msg,
                                        remaining_ms: state.theme.timing.status_flash_ms,
                                    };
                                    state.needs_redraw = true;
                                    } // end else (content not empty)
                                }
                                InputAction::ImageFormatError => {
                                    let fb = FeedbackBlock {
                                        id: generate_conversation_id(),
                                        message: "Unsupported image format. Supported: PNG, JPEG, GIF, WebP".to_string(),
                                        level: FeedbackLevel::Error,
                                        actions: vec![],
                                    };
                                    state.feedback_blocks.insert(fb.id.clone(), fb);
                                    state.needs_redraw = true;
                                }
                                InputAction::ImageSizeWarning { media_type, data, warning } => {
                                    // AC4: Show confirm/cancel FeedbackBlock for large images.
                                    // Store the image pending user confirmation.
                                    let attachment = ImageAttachment {
                                        media_type,
                                        data,
                                    };
                                    state.pending_large_image = Some(attachment);
                                    let fb = FeedbackBlock {
                                        id: generate_conversation_id(),
                                        message: warning,
                                        level: FeedbackLevel::Warning,
                                        actions: vec![
                                            FeedbackAction::Custom("[y] Attach anyway".to_string()),
                                            FeedbackAction::Custom("[n] Cancel".to_string()),
                                        ],
                                    };
                                    state.active_feedback_id = Some(fb.id.clone());
                                    state.feedback_blocks.insert(fb.id.clone(), fb);
                                    state.focus = FocusState::Chat;
                                    state.needs_redraw = true;
                                }
                                InputAction::ImageConfirmAttach => {
                                    // AC4: User confirmed attaching the large image
                                    if let Some(attachment) = state.pending_large_image.take() {
                                        use crate::adapters::tui::image;
                                        state.pending_images.push(attachment);
                                        state.image_indicator = Some(image::format_image_indicator(
                                            state.pending_images.len(),
                                            state.pending_images.iter().fold(0usize, |acc, i| acc.saturating_add(i.data.len() / 1024)),
                                        ));
                                    }
                                    // Clear the feedback block
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                }
                                InputAction::ImageConfirmCancel => {
                                    // AC4: User cancelled the large image attachment
                                    state.pending_large_image = None;
                                    // Clear the feedback block
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    state.status_before_flash = Some(state.status.clone());
                                    state.status = StatusState::Flash {
                                        message: "Image attachment cancelled".to_string(),
                                        remaining_ms: state.theme.timing.status_flash_ms,
                                    };
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                }
                                InputAction::NewTab => {
                                    // Abort any active streaming on this tab before saving,
                                    // so the saved state reflects the abort (is_streaming = false,
                                    // partial response finalized). Without this, Tab 1 is saved
                                    // with is_streaming = true but no task producing chunks → stuck.
                                    if streaming.is_streaming {
                                        // Finalize active tool calls with [aborted]
                                        for (_, tc) in streaming.active_tool_calls.iter_mut() {
                                            if tc.result.is_none() {
                                                tc.result = Some(crate::domain::models::ToolResultInfo {
                                                    content: "[aborted]".to_string(),
                                                    is_error: true,
                                                });
                                                tc.completed_at_ms = Some(now_unix() as u64 * 1000);
                                            }
                                        }
                                        // Preserve partial response as a finalized message
                                        if !streaming.current_text_buffer.is_empty()
                                            || !streaming.active_tool_calls.is_empty()
                                        {
                                            let content = std::mem::take(&mut streaming.current_text_buffer);
                                            conversation.messages.push(ChatMessage {
                                                id: generate_conversation_id(),
                                                role: MessageRole::Assistant,
                                                content,
                                                content_blocks: std::mem::take(&mut streaming.current_blocks),
                                                tool_calls: streaming.active_tool_calls.drain().map(|(_, v)| v).collect(),
                                                created_at: now_unix(),
                                                token_count: None,
                                                stop_reason: Some(crate::domain::models::StopReason::Cancelled),
                                            });
                                        }
                                        // Abort the streaming task
                                        if let Some(handle) = _active_turn.take() {
                                            handle.abort();
                                        }
                                        // Reset streaming state
                                        streaming.is_streaming = false;
                                        streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                        streaming.current_blocks.clear();
                                        streaming.active_tool_calls.clear();
                                        while turn_queue.dequeue().is_some() {}
                                    }
                                    // Save current tab state back to TabManager (after abort,
                                    // so stored state has is_streaming = false)
                                    save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                    // Persist current conversation if it has messages
                                    if !conversation.messages.is_empty() {
                                        conversation.updated_at = now_unix();
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
                                                Ok(Err(e)) => tracing::error!("Failed to save before new tab: {}", e),
                                                Err(_) => tracing::warn!("Tab save timed out"),
                                            }
                                        }));
                                    }
                                    // Create new tab in TabManager (switches active to the new tab)
                                    tab_manager.create_tab();
                                    // Load new tab state into proxies (fresh tab, never has queued messages)
                                    let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                    state.input_buffer.clear();
                                    state.cursor_position = 0;
                                    state.input_scroll_offset = 0;
                                    state.autocomplete.dismiss();
                                    state.resolved_mentions.clear();
                                    state.focus = FocusState::Input;
                                    state.status = StatusState::Idle;
                                    state.needs_redraw = true;
                                }
                                InputAction::CloseTab => {
                                    if tab_manager.tab_count() == 1 {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "Only one tab open".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                        state.needs_redraw = true;
                                    } else {
                                        // Save current tab state
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        let tab_id = tab_manager.active_tab_id();
                                        // Abort active streaming on this tab
                                        if streaming.is_streaming {
                                            if let Some(handle) = _active_turn.take() {
                                                handle.abort();
                                            }
                                            while turn_queue.dequeue().is_some() {}
                                        }
                                        // Persist conversation if it has messages
                                        if !conversation.messages.is_empty() {
                                            conversation.updated_at = now_unix();
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
                                                    Ok(Err(e)) => tracing::error!("Failed to save before close tab: {}", e),
                                                    Err(_) => tracing::warn!("Close-tab save timed out"),
                                                }
                                            }));
                                        }
                                        // Close the tab (TabManager adjusts active index)
                                        tab_manager.close_tab(tab_id);
                                        // Load the new active tab into proxies
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager);
                                            }
                                        }
                                        state.input_buffer.clear();
                                        state.cursor_position = 0;
                                        state.input_scroll_offset = 0;
                                        state.autocomplete.dismiss();
                                        state.resolved_mentions.clear();
                                        state.focus = FocusState::Input;
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::SwitchToNextTab => {
                                    if tab_manager.tab_count() > 1 {
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        tab_manager.switch_to_next();
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager);
                                            }
                                        }
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::SwitchToPrevTab => {
                                    if tab_manager.tab_count() > 1 {
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        tab_manager.switch_to_prev();
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager);
                                            }
                                        }
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::SwitchToTab(n) => {
                                    if tab_manager.tab_count() > 1
                                        && n >= 1
                                        && n <= tab_manager.tab_count()
                                        && n - 1 != tab_manager.active_tab_index()
                                    {
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        tab_manager.switch_to_index(n);
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager);
                                            }
                                        }
                                        state.needs_redraw = true;
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
                    AppEvent::ProviderChunk { conversation_id, chunk } => {
                        if conversation_id == conversation.id {
                            // Active tab — apply chunk to proxy variables as normal
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
                                        let title_conv_id = conversation.id.clone();
                                        tokio::spawn(async move {
                                            match tokio::time::timeout(
                                                BACKGROUND_TASK_TIMEOUT,
                                                generate_title(&*provider_ref, &model, &user_msg, &assistant_msg),
                                            ).await {
                                                Ok(Ok(title)) => {
                                                    let _ = event_tx_ref.send(AppEvent::TitleGenerated {
                                                        conversation_id: title_conv_id,
                                                        title,
                                                    });
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
                                            queued_msg.images,
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
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager)) {
                                            Ok(()) => state.needs_redraw = false,
                                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                        }
                                    }
                                }
                                ChunkAction::TurnContinuing => {
                                    state.status = StatusState::Executing {
                                        tool_name: "tools".to_string(),
                                        elapsed_ms: 0,
                                    };
                                    state.needs_redraw = true;
                                }
                                ChunkAction::None => {}
                            }
                        } else if let Some(tab) = tab_manager.find_by_conversation_mut(&conversation_id) {
                            // Background tab — route chunk directly to its stored state
                            let action = apply_chunk(
                                &mut tab.conversation,
                                &mut tab.streaming,
                                chunk,
                                now_unix(),
                            );
                            match action {
                                ChunkAction::TurnComplete { persist, trigger_title_generation } => {
                                    // apply_chunk already set tab.streaming.is_streaming = false
                                    if persist {
                                        let now = now_unix();
                                        tab.conversation.updated_at = now;
                                        tab.conversation.last_response_at = Some(now);
                                        if let Some(prev) = _pending_save.take() {
                                            let _ = prev.await;
                                        }
                                        let storage_ref = storage.clone();
                                        let conv_clone = tab.conversation.clone();
                                        _pending_save = Some(tokio::spawn(async move {
                                            match tokio::time::timeout(
                                                BACKGROUND_TASK_TIMEOUT,
                                                storage_ref.save_conversation(&conv_clone),
                                            ).await {
                                                Ok(Ok(())) => {}
                                                Ok(Err(e)) => tracing::error!("Failed to persist background tab: {}", e),
                                                Err(_) => tracing::warn!("Background tab save timed out"),
                                            }
                                        }));
                                    }
                                    if trigger_title_generation && tab.conversation.title.is_empty()
                                        && tab.conversation.messages.len() >= 2
                                    {
                                        let provider_ref = provider.clone();
                                        let event_tx_ref = domain_tx.clone();
                                        let model = config.model.clone();
                                        let user_msg = tab.conversation.messages[0].content.clone();
                                        let assistant_msg = truncate(&tab.conversation.messages[1].content, 500);
                                        let title_conv_id = tab.conversation.id.clone();
                                        tokio::spawn(async move {
                                            match tokio::time::timeout(
                                                BACKGROUND_TASK_TIMEOUT,
                                                generate_title(&*provider_ref, &model, &user_msg, &assistant_msg),
                                            ).await {
                                                Ok(Ok(title)) => {
                                                    let _ = event_tx_ref.send(AppEvent::TitleGenerated {
                                                        conversation_id: title_conv_id,
                                                        title,
                                                    });
                                                }
                                                Ok(Err(e)) => tracing::warn!("Background title gen failed: {}", e),
                                                Err(_) => tracing::warn!("Background title gen timed out"),
                                            }
                                        });
                                    }
                                    // Redraw tab bar — background tab may have gained a title
                                    state.needs_redraw = true;
                                }
                                // NeedsRedraw/TurnContinuing/None for background tab:
                                // user isn't viewing it, no UI update needed
                                _ => {}
                            }
                        }
                    }
                    AppEvent::SystemNotice { conversation_id: notice_conv_id, level, message: msg } => {
                        // Route by conversation_id: None = global (always active tab),
                        // Some(id) = only affects that conversation's tab.
                        let is_active_tab = notice_conv_id.as_deref()
                            .map_or(true, |id| id == conversation.id);

                        if is_active_tab {
                            // Detect session expiry from provider authentication errors.
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
                            if !matches!(level, crate::domain::models::NoticeLevel::Info) {
                                streaming.is_streaming = false;
                                streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                streaming.current_blocks.clear();
                                streaming.active_tool_calls.clear();
                                // Only abort _active_turn when it belongs to this tab
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
                                    state.focus = FocusState::Chat;
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
                        } else if let Some(id) = notice_conv_id {
                            // Background tab error — apply to its stored state in TabManager
                            if let Some(tab) = tab_manager.find_by_conversation_mut(&id) {
                                if !matches!(level, crate::domain::models::NoticeLevel::Info) {
                                    tab.streaming.is_streaming = false;
                                    tab.streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                    tab.streaming.current_blocks.clear();
                                    tab.streaming.active_tool_calls.clear();
                                }
                                if matches!(level, crate::domain::models::NoticeLevel::Error) {
                                    static BG_FB_COUNTER: AtomicUsize = AtomicUsize::new(0);
                                    let fb_id = format!("bgfb-{}", BG_FB_COUNTER.fetch_add(1, Ordering::Relaxed));
                                    let fb = FeedbackBlock {
                                        id: fb_id.clone(),
                                        level: FeedbackLevel::Error,
                                        message: msg,
                                        actions: vec![FeedbackAction::Retry],
                                    };
                                    tab.feedback_blocks.insert(fb_id.clone(), fb);
                                    tab.active_feedback_id = Some(fb_id);
                                    tab.streaming.is_streaming = false;
                                }
                                // Redraw so tab bar can reflect the state change
                                state.needs_redraw = true;
                            }
                        }
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
                    AppEvent::PermissionRequestForConv {
                        conversation_id,
                        tool_name,
                        tool_input,
                        response_tx,
                    } => {
                        // Route permission request to the correct tab
                        if conversation_id == conversation.id {
                            // For active tab, display immediately
                            use crate::adapters::tui::state::PendingPermission;
                            let new_pending = PendingPermission {
                                tool_name,
                                tool_input,
                                response_tx,
                            };
                            if state.pending_permission.is_some() {
                                state.permission_queue.push(new_pending);
                            } else {
                                state.pending_permission = Some(new_pending);
                                state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                    ConfirmationType::Permission,
                                ));
                            }
                            state.needs_redraw = true;
                        } else if let Some(_tab) = tab_manager.find_by_conversation_mut(&conversation_id) {
                            // For background tab, store in tab state for later display
                            use crate::adapters::tui::state::PendingPermission;
                            let new_pending = PendingPermission {
                                tool_name,
                                tool_input,
                                response_tx,
                            };
                            // Note: TabState doesn't have pending_permission field yet
                            // For now, we queue it in the global state but mark it with conversation_id
                            // TODO: Add pending_permission to TabState in future refactor
                            tracing::warn!("Permission request for background tab {} queued", conversation_id);
                            // For now, treat as active tab (temporary until full multi-tab permission state)
                            if state.pending_permission.is_some() {
                                state.permission_queue.push(new_pending);
                            } else {
                                state.pending_permission = Some(new_pending);
                                state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                    ConfirmationType::Permission,
                                ));
                            }
                            state.needs_redraw = true;
                        }
                        // Silently drop if conversation not found (tab closed)
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
                    AppEvent::RetryMessage { content: text, .. } => {
                        // Delayed retry arrived — start the turn now (no images on retry)
                        state.status = StatusState::Streaming;
                        start_turn(
                            &text,
                            vec![],
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
                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager)) {
                            Ok(()) => state.needs_redraw = false,
                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                        }
                    }
                    AppEvent::TitleGenerated { conversation_id, title } => {
                        if conversation_id == conversation.id {
                            // Active tab
                            conversation.title = title;
                            state.needs_redraw = true;
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
                                    Ok(Err(e)) => tracing::error!("Failed to persist title: {}", e),
                                    Err(_) => tracing::warn!("Background task 'title_save' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs()),
                                }
                            }));
                        } else if let Some(tab) = tab_manager.find_by_conversation_mut(&conversation_id) {
                            // Background tab — update its stored conversation title
                            tab.conversation.title = title;
                            if let Some(prev) = _pending_save.take() {
                                let _ = prev.await;
                            }
                            let storage_ref = storage.clone();
                            let conv_clone = tab.conversation.clone();
                            _pending_save = Some(tokio::spawn(async move {
                                match tokio::time::timeout(
                                    BACKGROUND_TASK_TIMEOUT,
                                    storage_ref.save_conversation(&conv_clone),
                                ).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => tracing::error!("Failed to persist background tab title: {}", e),
                                    Err(_) => tracing::warn!("Background tab title save timed out"),
                                }
                            }));
                            // Redraw tab bar to show the new title
                            state.needs_redraw = true;
                        }
                    }
                    AppEvent::AskUserQuestion {
                        conversation_id,
                        tool_use_id,
                        question,
                        response_tx,
                    } => {
                        // Only display question if it belongs to the active tab
                        if conversation_id == conversation.id {
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
                        } else if let Some(_tab) = tab_manager.find_by_conversation_mut(&conversation_id) {
                            // Background tab: store for later (when tab becomes active)
                            // TODO: Add ask_user_question state to TabState for proper routing
                            tracing::warn!("AskUserQuestion for background tab {} - storing for later", conversation_id);
                            // For now, show it immediately (temporary until full per-tab state)
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
                        // Silently drop if conversation not found (tab closed)
                    }
                    AppEvent::RecoveryPrompt { title, token_count, .. } => {
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

                // Which-key timeout check: auto-dismiss if expired (AC6)
                if state.which_key.active && state.which_key.is_timed_out(state.theme.timing.which_key_timeout_ms) {
                    let prev = state.which_key.dismiss();
                    if let Some(focus) = prev {
                        state.focus = focus;
                    }
                    state.needs_redraw = true;
                }

                if state.needs_redraw {
                    match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager)) {
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

    // Persist active tab on shutdown with clean_exit = true (graceful shutdown)
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
                tracing::warn!(
                    "Session save timed out on shutdown (>2s), proceeding with teardown"
                );
            }
        }
    }
    // Persist all background (non-active) tabs on shutdown
    let active_conv_id = conversation.id.clone();
    for tab in tab_manager.tabs() {
        if tab.conversation.id != active_conv_id && !tab.conversation.messages.is_empty() {
            let mut bg_conv = tab.conversation.clone();
            bg_conv.updated_at = now_unix();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                fs_storage.save_conversation_with_exit(&bg_conv, true),
            )
            .await;
        }
    }

    Ok(())
}

/// Save the active-tab proxy state into TabManager before switching tabs.
fn save_active_tab(
    tab_manager: &mut TabManager,
    conversation: &Conversation,
    streaming: &StreamingState,
    session_manager: &SessionManager,
    state: &TuiState,
    turn_queue: &TurnQueue,
) {
    let tab = tab_manager.active_tab_mut();
    tab.conversation = conversation.clone();
    tab.streaming = streaming.clone();
    tab.session = session_manager.clone();
    tab.scroll_offset = state.scroll_offset;
    tab.auto_scroll = state.auto_scroll;
    tab.block_boundaries = state.block_boundaries.clone();
    tab.message_boundaries = state.message_boundaries.clone();
    tab.focused_tool_id = state.focused_tool_id.clone();
    tab.feedback_blocks = state.feedback_blocks.clone();
    tab.active_feedback_id = state.active_feedback_id.clone();
    tab.total_content_height = state.total_content_height;
    tab.pending_anchor = state.pending_anchor;
    tab.turn_queue = turn_queue.clone();
}

/// Load the new active tab's state from TabManager into the proxy variables.
/// Returns `true` if the tab is idle and has queued messages ready to send
/// (e.g. a turn completed in the background while the tab was inactive).
fn load_active_tab(
    tab_manager: &TabManager,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    session_manager: &mut SessionManager,
    state: &mut TuiState,
    turn_queue: &mut TurnQueue,
) -> bool {
    let tab = tab_manager.active_tab();
    *conversation = tab.conversation.clone();
    *streaming = tab.streaming.clone();
    *session_manager = tab.session.clone();
    *turn_queue = tab.turn_queue.clone();
    state.scroll_offset = tab.scroll_offset;
    // If the tab is not actively streaming, snap to bottom so the user always lands
    // at the latest content (complete response or empty tab). If still streaming,
    // preserve the user's scroll position (they may have scrolled up intentionally).
    state.auto_scroll = if !tab.streaming.is_streaming {
        true
    } else {
        tab.auto_scroll
    };
    state.block_boundaries = tab.block_boundaries.clone();
    state.message_boundaries = tab.message_boundaries.clone();
    state.focused_tool_id = tab.focused_tool_id.clone();
    state.feedback_blocks = tab.feedback_blocks.clone();
    state.active_feedback_id = tab.active_feedback_id.clone();
    state.total_content_height = tab.total_content_height;
    state.pending_anchor = tab.pending_anchor;
    // Reset per-tab renderer caches — rebuilt on next render
    state.height_cache.invalidate_all();
    state.tool_block_states.clear();
    // Sync token usage from the loaded conversation
    state.token_usage = tab.conversation.usage.clone();
    // Infer status from loaded streaming state so the status bar reflects the tab being entered.
    // Without this, a stale "Streaming" status from the departing tab persists on screen
    // even after the background turn completed (the background TurnComplete handler intentionally
    // does not touch state.status because it belongs to a different tab).
    state.status = if tab.streaming.is_streaming {
        StatusState::Streaming
    } else {
        StatusState::Idle
    };
    // Clear any retry state that belonged to the previous tab
    state.retry_state = None;
    // Signal caller if queued messages should be drained: the tab's turn completed
    // in the background but auto-send (line 948) only runs in the active-tab path.
    !tab.streaming.is_streaming && !tab.turn_queue.is_empty()
}

/// Start a new turn: add user message, spawn provider streaming task.
#[allow(clippy::too_many_arguments)]
fn start_turn(
    text: &str,
    images: Vec<ImageAttachment>,
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
        id: generate_conversation_id(),
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

    // Attach images to the last user message in the API request
    // Covers: FR112 (AC1, AC2)
    if !images.is_empty() {
        if let Some(last_user_msg) = messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::User && m.content == text)
        {
            last_user_msg.images = images;
        }
    }

    // If session manager indicates history rebuild needed, prepend context
    if session_manager.needs_history_rebuild() {
        use crate::domain::services::history_rebuild;
        let context = history_rebuild::build_history_context(
            &conversation.messages[..conversation.messages.len().saturating_sub(1)],
        );
        // Attach context_prefix to the actual user input message (matching `text`),
        // not to a synthetic tool-result message that build_api_messages may have appended.
        if let Some(target_msg) = messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::User && m.content == text)
        {
            target_msg.context_prefix = Some(context);
        } else if let Some(last_msg) = messages.last_mut() {
            // Fallback: attach to last message if exact match not found
            last_msg.context_prefix = Some(context);
        }
        let new_session_id = generate_conversation_id();
        conversation.session_id = Some(new_session_id.clone());
        session_manager.mark_active(new_session_id);
        let msg_count = conversation.messages.len().saturating_sub(1);
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: Some(conversation.id.clone()),
            level: crate::domain::models::NoticeLevel::Info,
            message: format!(
                "\u{2139}\u{fe0f} Session restarted with your conversation history ({} messages).",
                msg_count
            ),
        });
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
        conversation.id.clone(),
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
#[allow(clippy::too_many_arguments)]
fn render(
    terminal: &mut Tui,
    state: &mut TuiState,
    conversation: &Conversation,
    streaming: &StreamingState,
    model: &str,
    security: &dyn SecurityPort,
    tab_count: usize,
    active_tab_index: usize,
    tab_manager_for_bar: Option<&crate::domain::models::tab::TabManager>,
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
        ref autocomplete,
        ref command_palette,
        ref which_key,
        ref help_overlay,
        ref image_indicator,
        ref current_hint,
        ..
    } = *state;

    terminal.draw(|frame| {
        let area = frame.area();

        match layout::compute_layout(area, theme, input_buffer, tab_count) {
            Some(app_layout) => {
                let _is_compact = area.width < 80 || area.height < 24;

                // Render tab bar if present
                if let (Some(tab_bar_area), Some(tm)) = (app_layout.tab_bar, tab_manager_for_bar) {
                    use crate::adapters::tui::widgets::tab_bar;
                    tab_bar::render_tab_bar(
                        tm,
                        active_tab_index,
                        tab_bar_area,
                        frame.buffer_mut(),
                        theme,
                    );
                }

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
                    current_hint.as_deref(),
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
                    image_indicator.as_deref(),
                );

                // Render reverse search overlay above input box
                if reverse_search.active {
                    reverse_search::render(frame, app_layout.input_area, reverse_search, theme);
                }

                // Render autocomplete popup above input box
                if autocomplete.active {
                    autocomplete_popup::render(frame, app_layout.input_area, autocomplete, theme);
                }

                // Render command palette overlay (centered, on top of everything)
                if command_palette.active {
                    command_palette_widget::render(frame, area, command_palette, theme);
                }

                // Render which-key hint bar (bottom of screen)
                if which_key.active {
                    which_key_bar::render(frame, area, which_key, theme);
                }

                // Render help overlay (full-screen modal, on top of everything)
                // Covers: FR108, UX-DR94
                if help_overlay.active {
                    help_overlay::render(
                        frame,
                        area,
                        help_overlay,
                        theme,
                        state.multiplexer_detected,
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

    let prompt_content = format!("User: {}\n\nAssistant: {}", user_msg, assistant_msg);
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

/// Populate autocomplete suggestions based on current state.
/// Called after handle_input when autocomplete is active.
fn populate_autocomplete_suggestions(
    state: &mut TuiState,
    command_registry: &mut CommandRegistry,
    workspace_path: &std::path::Path,
) {
    use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};

    match state.autocomplete.kind {
        AutocompleteKind::SlashCommand => {
            // Lazy-load command discovery on first slash
            if !command_registry.is_discovered() {
                command_registry.discover_into(workspace_path);
            }
            let filtered = command_registry.filter(&state.autocomplete.filter_text);
            state.autocomplete.suggestions = filtered
                .into_iter()
                .map(|cmd| AutocompleteSuggestion::SlashCommand {
                    name: cmd.name.clone(),
                    description: cmd.description.clone(),
                })
                .collect();
            // Reset selection if suggestions changed
            if state.autocomplete.selected_index >= state.autocomplete.suggestions.len() {
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
            }
        }
        AutocompleteKind::FileMention => {
            let results = file_scanner::scan_workspace_files(
                workspace_path,
                &state.autocomplete.filter_text,
                50,
            );
            state.autocomplete.suggestions = results
                .into_iter()
                .map(|f| AutocompleteSuggestion::FilePath {
                    path: f.relative_path,
                    is_dir: f.is_dir,
                })
                .collect();
            if state.autocomplete.selected_index >= state.autocomplete.suggestions.len() {
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
            }
        }
    }
}

/// Populate command palette filtered entries from the registry.
/// Called after handle_input when palette is active and filter text changes.
fn populate_palette_entries(
    state: &mut TuiState,
    command_registry: &mut CommandRegistry,
    palette_registry: &mut PaletteRegistry,
    workspace_path: &std::path::Path,
) {
    // Ensure command registry is discovered (lazy-load)
    if !command_registry.is_discovered() {
        command_registry.discover_into(workspace_path);
    }

    // Populate palette from command registry (cached, only rebuilds if discovery state changed)
    palette_registry.populate_from_command_registry(command_registry);

    // Determine scope from filter prefix
    let (scope, query) = if let Some(palette_scope) = state.command_palette.current_scope {
        // Strip prefix character from query
        let q = if state.command_palette.filter_text.len() > 1 {
            state
                .command_palette
                .filter_text
                .chars()
                .skip(1)
                .collect::<String>()
        } else {
            String::new()
        };
        (Some(palette_scope), q)
    } else {
        (None, state.command_palette.filter_text.clone())
    };

    // Fuzzy filter entries
    let filtered = palette_registry.fuzzy_filter(&query, scope);
    let entries: Vec<_> = filtered.into_iter().cloned().collect();
    state.command_palette.filtered_entries = entries;

    // Clamp selection and scroll_offset to valid range after filter update
    let max_idx = state
        .command_palette
        .filtered_entries
        .len()
        .saturating_sub(1);
    state.command_palette.selected_index = state.command_palette.selected_index.min(max_idx);
    state.command_palette.scroll_offset = state.command_palette.scroll_offset.min(max_idx);
}

/// Resolve file mentions and build context prefix for a message submission.
/// Reads file contents in the adapter layer (no file I/O in domain).
/// Errors encountered during file context resolution, for FeedbackBlock display.
struct FileContextError {
    #[allow(dead_code)]
    path: String,
    reason: String,
}

/// Resolve what content to copy based on current focus state.
/// Priority: focused tool block output > last assistant message > empty.
// Covers: FR116 (AC6, AC8, AC9)
fn resolve_copy_content(state: &TuiState, conversation: &Conversation) -> String {
    // AC8: If a tool block is focused, copy its output
    if let Some(ref tool_id) = state.focused_tool_id {
        // Find the tool result in conversation messages
        for cm in conversation.messages.iter().rev() {
            for tc in &cm.tool_calls {
                if tc.id == *tool_id {
                    if let Some(ref result) = tc.result {
                        return result.content.clone();
                    }
                }
            }
        }
    }

    // AC9: Copy the last assistant message
    for cm in conversation.messages.iter().rev() {
        if cm.role == MessageRole::Assistant && !cm.content.is_empty() {
            return cm.content.clone();
        }
    }

    String::new()
}

fn resolve_file_context(
    mentions: &[crate::adapters::tui::state::ResolvedMention],
    workspace_path: &std::path::Path,
    security: &dyn SecurityPort,
) -> (
    Vec<message_builder::ResolvedFileContext>,
    Vec<ImageAttachment>,
    Vec<FileContextError>,
) {
    use crate::adapters::tui::image;
    use crate::domain::models::FileOperation;

    let mut resolved = Vec::new();
    let mut images = Vec::new();
    let mut errors = Vec::new();
    for mention in mentions {
        let full_path = workspace_path.join(&mention.path);

        // P3: Validate path is within workspace before reading
        let canonical = match full_path.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                errors.push(FileContextError {
                    path: mention.path.clone(),
                    reason: format!("File not found: {}", mention.path),
                });
                continue;
            }
        };
        if let Err(e) = security.check_workspace_access(&canonical, FileOperation::Read) {
            tracing::warn!("Security check failed for mention {}: {}", mention.path, e);
            errors.push(FileContextError {
                path: mention.path.clone(),
                reason: format!("Access denied: {}", mention.path),
            });
            continue;
        }

        // Check if file is an image by extension
        // Covers: FR112 (AC2, AC3)
        let is_image = canonical
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| image::is_image_extension(ext));

        if is_image {
            // P2: Check file size before reading to prevent OOM on large images
            const MAX_IMAGE_FILE_SIZE: u64 = 20 * 1024 * 1024; // 20MB
            match std::fs::metadata(&canonical) {
                Ok(meta) if meta.len() > MAX_IMAGE_FILE_SIZE => {
                    errors.push(FileContextError {
                        path: mention.path.clone(),
                        reason: format!(
                            "Image file too large ({:.1}MB). Maximum: 20MB",
                            meta.len() as f64 / (1024.0 * 1024.0)
                        ),
                    });
                    continue;
                }
                Err(_) => {
                    errors.push(FileContextError {
                        path: mention.path.clone(),
                        reason: format!("Could not read file: {}", mention.path),
                    });
                    continue;
                }
                _ => {} // Size OK, proceed
            }
            // Read as bytes, validate format, base64-encode
            match std::fs::read(&canonical) {
                Ok(bytes) => match image::detect_image_format(&bytes) {
                    Ok(media_type) => {
                        use base64::Engine;
                        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        images.push(ImageAttachment {
                            media_type: media_type.to_string(),
                            data,
                        });
                    }
                    Err(msg) => {
                        errors.push(FileContextError {
                            path: mention.path.clone(),
                            reason: msg,
                        });
                    }
                },
                Err(_) => {
                    errors.push(FileContextError {
                        path: mention.path.clone(),
                        reason: format!("Could not read file: {}", mention.path),
                    });
                }
            }
        } else {
            // Non-image files: read as text (existing behavior)
            match std::fs::read_to_string(&canonical) {
                Ok(content) => {
                    // P2: Limit to ~100KB per file — use floor_char_boundary to avoid UTF-8 panic
                    #[allow(clippy::incompatible_msrv)] // stable since 1.80, MSRV is 1.85
                    let truncated = if content.len() > 100_000 {
                        let boundary = content.floor_char_boundary(100_000);
                        content[..boundary].to_string()
                    } else {
                        content
                    };
                    resolved.push(message_builder::ResolvedFileContext {
                        path: mention.path.clone(),
                        content: truncated,
                    });
                }
                Err(_) => {
                    errors.push(FileContextError {
                        path: mention.path.clone(),
                        reason: format!("File not found: {}", mention.path),
                    });
                }
            }
        }
    }
    (resolved, images, errors)
}

/// Resolve a user-defined slash command's content.
fn resolve_command_context(
    cmd_name: &str,
    command_registry: &CommandRegistry,
) -> Option<message_builder::ResolvedCommandContext> {
    let cmd_def = command_registry.find(cmd_name)?;
    let content = cmd_def.content.as_ref()?;
    Some(message_builder::ResolvedCommandContext {
        name: cmd_name.to_string(),
        content: content.clone(),
    })
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
