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
    input_box, reverse_search, sidebar, status_bar, which_key_bar,
};
use crate::domain::services::session_index::SessionIndex;

/// Timeout for background tasks (title generation, session save).
/// Separate from shutdown persist timeout (2s) which is more critical.
const BACKGROUND_TASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
use crate::domain::events::{AppEvent, ChunkAction};
use crate::domain::models::tab::TabManager;
use crate::domain::models::visual::{ConfirmationType, DeleteConfirmTarget, OverlayType};
use crate::domain::models::{
    AppConfig, ApprovalDecision, ChatMessage, CompletionOptions, Conversation, FeedbackAction,
    FeedbackBlock, FeedbackLevel, FocusState, ImageAttachment, MessageRole, RetryState,
    SessionManager, SessionState, StatusState, StreamChunk, StreamingState, UserMessage,
    apply_chunk, generate_conversation_id, next_delay,
};
use crate::domain::ports::{
    ClipboardPort, PersonaPort, ProviderPort, SecurityPort, StoragePort, ToolSetPort,
};
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
    clipboard: Arc<dyn ClipboardPort>,
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

    // Story 4-4 AC8/AC9: hydrate session_meta for the restored conversation
    // so bookmarks from previous sessions survive reload. Failure is silent —
    // fresh/new conversations have no meta.json yet and that's the correct
    // default (empty bookmarks).
    {
        let conv_id = tab_manager.active_tab().conversation.id.clone();
        if let Ok(Some(meta)) = fs_storage.load_session_meta(&conv_id).await {
            tab_manager.active_tab_mut().session_meta = meta;
        }
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

    // P2/AC9: Build SessionIndex at startup from persisted session metadata
    let mut session_index = match storage.list_conversations().await {
        Ok(summaries) => SessionIndex::build(summaries),
        Err(e) => {
            tracing::warn!("Failed to build session index at startup: {}", e);
            SessionIndex::new()
        }
    };
    // Mark the active conversation as open and active in the index
    session_index.set_open(&conversation.id, true);
    session_index.set_active(Some(&conversation.id));
    state.sidebar_entry_count = session_index.len();

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
        &session_index,
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
                                        conversation.created_at = crate::domain::models::session_meta::now_unix();
                                        conversation.updated_at = crate::domain::models::session_meta::now_unix();
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
                                            &fs_storage,
                                            &storage,
                                        );
                                        // Force immediate render for typing indicator
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
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
                                                tc.completed_at_ms = Some(crate::domain::models::session_meta::now_unix() as u64 * 1000);
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
                                                created_at: crate::domain::models::session_meta::now_unix(),
                                                token_count: None,
                                                stop_reason: Some(crate::domain::models::StopReason::Cancelled),
                                                images: vec![],
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
                                InputAction::ExecuteCommand { name: cmd_name, args: cmd_args } => {
                                    let cmd_arg: Option<&str> = cmd_args.as_deref();
                                    let cmd_name: &str = cmd_name.as_str();
                                    if cmd_name == "ml" {
                                        // /ml command: toggle multi-line mode (AC#3)
                                        // Covers: Sprint Change Proposal 2026-04-08
                                        state.multiline_mode = !state.multiline_mode;
                                        state.needs_redraw = true;
                                    } else if cmd_name == "export" {
                                        apply_export_command(
                                            cmd_arg,
                                            &conversation,
                                            &tab_manager.active_tab().session_meta.clone(),
                                            &workspace_path,
                                            &mut state,
                                        )
                                        .await;
                                    } else if cmd_name == "new" {
                                        // /new command: save current, create fresh session
                                        // AC7: save current conversation if it has messages
                                        if !conversation.messages.is_empty() {
                                            conversation.updated_at = crate::domain::models::session_meta::now_unix();
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
                                            created_at: crate::domain::models::session_meta::now_unix(),
                                            updated_at: crate::domain::models::session_meta::now_unix(),
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
                                        state.user_message_boundaries.clear();
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
                                            &fs_storage,
                                            &storage,
                                        );
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
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
                                InputAction::RequestClipboardPaste => {
                                    // Async: read image (or text) from the OS clipboard via
                                    // ClipboardPort, then re-enter handle_input so all existing
                                    // image validation / state mutation runs exactly once.
                                    use crate::domain::events::DomainInputEvent;
                                    match clipboard.read_image_png().await {
                                        Ok(Some(png_bytes)) => {
                                            let inner_action = handle_input(
                                                &mut state,
                                                &DomainInputEvent::ImagePaste(png_bytes),
                                            );
                                            // Propagate image-validation outcomes inline
                                            match inner_action {
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
                                                    let attachment = ImageAttachment { media_type, data };
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
                                                _ => {} // Consumed or other — already mutated state
                                            }
                                        }
                                        Ok(None) => {
                                            // No image: try text
                                            match clipboard.read_text().await {
                                                Ok(Some(text)) => {
                                                    handle_input(
                                                        &mut state,
                                                        &DomainInputEvent::Paste(text),
                                                    );
                                                }
                                                Ok(None) => {
                                                    state.status_before_flash = Some(state.status.clone());
                                                    state.status = StatusState::Flash {
                                                        message: "Clipboard is empty".to_string(),
                                                        remaining_ms: state.theme.timing.status_flash_ms,
                                                    };
                                                    state.needs_redraw = true;
                                                }
                                                Err(e) => {
                                                    let fb = FeedbackBlock {
                                                        id: generate_conversation_id(),
                                                        message: format!("Could not read clipboard: {e}"),
                                                        level: FeedbackLevel::Error,
                                                        actions: vec![],
                                                    };
                                                    state.feedback_blocks.insert(fb.id.clone(), fb);
                                                    state.needs_redraw = true;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let fb = FeedbackBlock {
                                                id: generate_conversation_id(),
                                                message: format!("Could not read clipboard: {e}"),
                                                level: FeedbackLevel::Error,
                                                actions: vec![],
                                            };
                                            state.feedback_blocks.insert(fb.id.clone(), fb);
                                            state.needs_redraw = true;
                                        }
                                    }
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
                                                tc.completed_at_ms = Some(crate::domain::models::session_meta::now_unix() as u64 * 1000);
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
                                                created_at: crate::domain::models::session_meta::now_unix(),
                                                token_count: None,
                                                stop_reason: Some(crate::domain::models::StopReason::Cancelled),
                                                images: vec![],
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
                                        conversation.updated_at = crate::domain::models::session_meta::now_unix();
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
                                    // Update sidebar: mark new conversation open/active
                                    session_index.set_open(&conversation.id, true);
                                    session_index.set_active(Some(&conversation.id));
                                    state.sidebar_entry_count = session_index.len();
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
                                        let closing_conv_id = conversation.id.clone();
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
                                            conversation.updated_at = crate::domain::models::session_meta::now_unix();
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
                                                start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage);
                                            }
                                        }
                                        // Update sidebar: mark closed tab as no longer open
                                        session_index.set_open(&closing_conv_id, false);
                                        session_index.set_active(Some(&conversation.id));
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
                                                start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage);
                                            }
                                        }
                                        session_index.set_active(Some(&conversation.id));
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
                                                start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage);
                                            }
                                        }
                                        session_index.set_active(Some(&conversation.id));
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
                                                start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage);
                                            }
                                        }
                                        session_index.set_active(Some(&conversation.id));
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::ToggleSidebar => {
                                    // P10: Don't toggle sidebar when an overlay is active
                                    if matches!(state.focus, FocusState::Overlay(_)) {
                                        // Ignore — overlay has focus priority
                                    } else if state.terminal_width >= layout::SIDEBAR_MIN_WIDTH {
                                        state.sidebar_visible = !state.sidebar_visible;
                                        if state.sidebar_visible {
                                            state.sidebar_panel = Some(crate::domain::models::visual::PanelType::History);
                                            state.focus = FocusState::Sidebar {
                                                panel: crate::domain::models::visual::PanelType::History,
                                                selected: state.sidebar_selected
                                            };
                                        } else {
                                            state.sidebar_panel = None;
                                            state.focus = FocusState::Chat;
                                        }
                                        state.needs_redraw = true;
                                    } else {
                                        // Terminal too narrow - show info message
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = crate::domain::models::StatusState::Flash {
                                            message: "Terminal too narrow for sidebar. Use Ctrl+P to search history.".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::OpenSidebarConversation => {
                                    // Resolve conversation ID from sidebar selection
                                    let resolved_id = session_index.entries()
                                        .get(state.sidebar_selected)
                                        .map(|e| e.conversation_id.clone());
                                    if let Some(conv_id) = resolved_id {
                                        // Check if already open in a tab
                                        if conv_id == conversation.id {
                                            // Already active — just switch focus
                                            state.focus = FocusState::Chat;
                                            state.needs_redraw = true;
                                        } else if tab_manager.find_by_conversation(&conv_id).is_some() {
                                            // Open in another tab — switch to it
                                            save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                            if let Some(idx) = tab_manager.tabs().iter().position(|t| t.conversation.id == conv_id) {
                                                tab_manager.switch_to_index(idx + 1); // 1-based
                                                let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                                if should_drain {
                                                    if let Some(queued_msg) = turn_queue.dequeue() {
                                                        start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage);
                                                    }
                                                }
                                                session_index.set_active(Some(&conv_id));
                                            }
                                            state.focus = FocusState::Chat;
                                            state.needs_redraw = true;
                                        } else {
                                            // Not open — load from storage into a new tab
                                            match storage.load_conversation(&conv_id).await {
                                                Ok(Some(loaded_conv)) => {
                                                    // Save current tab state first
                                                    save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                    // Create new tab and load the conversation into it
                                                    tab_manager.create_tab();
                                                    let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                                    // Overwrite the fresh conversation with the loaded one
                                                    conversation = loaded_conv;
                                                    // Story 4-4: hydrate session_meta (bookmarks) for the loaded tab
                                                    if let Ok(Some(meta)) = fs_storage.load_session_meta(&conv_id).await {
                                                        tab_manager.active_tab_mut().session_meta = meta;
                                                    }
                                                    // Save loaded conversation back into TabManager so it's not lost on tab switch
                                                    save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                    // Update session_index
                                                    session_index.set_open(&conv_id, true);
                                                    session_index.set_active(Some(&conv_id));
                                                    state.focus = FocusState::Chat;
                                                    state.needs_redraw = true;
                                                }
                                                Ok(None) => {
                                                    tracing::warn!("Sidebar: conversation {} not found in storage", conv_id);
                                                    session_index.remove(&conv_id);
                                                    state.sidebar_entry_count = session_index.len();
                                                    // Clamp sidebar selection after removal
                                                    if state.sidebar_entry_count > 0 {
                                                        state.sidebar_selected = state.sidebar_selected.min(state.sidebar_entry_count - 1);
                                                    } else {
                                                        state.sidebar_selected = 0;
                                                    }
                                                    state.needs_redraw = true;
                                                }
                                                Err(e) => {
                                                    tracing::error!("Failed to load conversation {}: {}", conv_id, e);
                                                    state.status_before_flash = Some(state.status.clone());
                                                    state.status = StatusState::Flash {
                                                        message: format!("Failed to load conversation: {}", e),
                                                        remaining_ms: state.theme.timing.status_flash_ms,
                                                    };
                                                    state.needs_redraw = true;
                                                }
                                            }
                                        }
                                    }
                                }
                                InputAction::DeleteSidebarConversation => {
                                    // Resolve conversation ID from sidebar selection
                                    if let Some(entry) = session_index.entries().get(state.sidebar_selected) {
                                        let conv_id = entry.conversation_id.clone();
                                        let title = entry.title.clone();

                                        // Don't delete the active conversation if it's the only tab
                                        if conv_id == conversation.id && tab_manager.tab_count() == 1 {
                                            state.status = StatusState::Flash {
                                                message: "Cannot delete the only open conversation".to_string(),
                                                remaining_ms: state.theme.timing.status_flash_ms,
                                            };
                                        } else {
                                            // Show confirmation prompt (AC5)
                                            let display_title = if title.is_empty() { "Untitled".to_string() } else { title };
                                            state.pending_delete = Some(DeleteConfirmTarget::Single {
                                                id: conv_id.clone(),
                                                title: display_title.clone()
                                            });
                                            state.status = StatusState::Flash {
                                                message: format!("Delete \"{}\"? This cannot be undone. [y/n]", display_title),
                                                remaining_ms: 30_000, // Long timeout for confirmation
                                            };
                                            state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                                ConfirmationType::DeleteConfirmation(DeleteConfirmTarget::Single {
                                                    id: conv_id.clone(),
                                                    title: display_title
                                                })
                                            ));
                                        }
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::DeleteAllConversations => {
                                    // Show confirmation prompt (AC6)
                                    let count = session_index.len();
                                    if count == 0 {
                                        state.status = StatusState::Flash {
                                            message: "No conversations to delete".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                    } else {
                                        state.pending_delete = Some(DeleteConfirmTarget::Bulk { count });
                                        state.status = StatusState::Flash {
                                            message: format!("Delete {} conversations? [y/n]", count),
                                            remaining_ms: 30_000,
                                        };
                                        state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                            ConfirmationType::DeleteConfirmation(DeleteConfirmTarget::Bulk { count })
                                        ));
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::ConfirmDelete => {
                                    // Execute the pending delete
                                    if let Some(target) = state.pending_delete.take() {
                                        match target {
                                            DeleteConfirmTarget::Single { id: conv_id, .. } => {
                                                // Single conversation delete
                                                if conv_id == conversation.id {
                                                    let tab_id = tab_manager.active_tab_id();
                                                    if streaming.is_streaming {
                                                        if let Some(handle) = _active_turn.take() {
                                                            handle.abort();
                                                        }
                                                        while turn_queue.dequeue().is_some() {}
                                                    }
                                                    tab_manager.close_tab(tab_id);
                                                    let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                                    if should_drain {
                                                        if let Some(queued_msg) = turn_queue.dequeue() {
                                                            start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage);
                                                        }
                                                    }
                                                    session_index.set_active(Some(&conversation.id));
                                                } else if let Some(tab) = tab_manager.tabs().iter().find(|t| t.conversation.id == conv_id) {
                                                    let tab_id = tab.id;
                                                    tab_manager.close_tab(tab_id);
                                                }
                                                let fs_ref = fs_storage.clone();
                                                let del_id = conv_id.clone();
                                                tokio::spawn(async move {
                                                    if let Err(e) = fs_ref.delete_conversation(&del_id).await {
                                                        tracing::error!("Failed to delete conversation {}: {}", del_id, e);
                                                    }
                                                });
                                                session_index.remove(&conv_id);
                                                state.sidebar_entry_count = session_index.len();
                                                if state.sidebar_entry_count > 0 {
                                                    state.sidebar_selected = state.sidebar_selected.min(state.sidebar_entry_count - 1);
                                                } else {
                                                    state.sidebar_selected = 0;
                                                }
                                                // U1: Clear status bar immediately after successful delete
                                                state.status = StatusState::Idle;
                                            }
                                            DeleteConfirmTarget::Bulk { .. } => {
                                                // Delete all conversations - F16: Fix race by clearing index BEFORE spawning task
                                                if streaming.is_streaming {
                                                    if let Some(handle) = _active_turn.take() {
                                                        handle.abort();
                                                    }
                                                    streaming.is_streaming = false;
                                                    streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                                    streaming.current_blocks.clear();
                                                    streaming.active_tool_calls.clear();
                                                    while turn_queue.dequeue().is_some() {}
                                                }
                                                // F16: Collect IDs and clear index atomically BEFORE spawning background task
                                                let conv_ids: Vec<String> = session_index.entries().iter()
                                                    .map(|e| e.conversation_id.clone())
                                                    .collect();
                                                // Clear the index immediately to prevent race with new conversations
                                                session_index.clear();
                                                state.sidebar_entry_count = 0;
                                                state.sidebar_selected = 0;
                                                // F14: Reset sidebar_scroll_offset during DeleteAll
                                                state.sidebar_scroll_offset = 0;

                                                let fs_ref = fs_storage.clone();
                                                tokio::spawn(async move {
                                                    for id in conv_ids {
                                                        if let Err(e) = fs_ref.delete_conversation(&id).await {
                                                            tracing::error!("Failed to delete conversation {}: {}", id, e);
                                                        }
                                                    }
                                                });

                                                while tab_manager.tab_count() > 1 {
                                                    let tab_id = tab_manager.tabs().last().map(|t| t.id).unwrap();
                                                    tab_manager.close_tab(tab_id);
                                                }
                                                conversation.messages.clear();
                                                conversation.title = String::new();
                                                conversation.id = generate_conversation_id();
                                                conversation.session_id = Some(generate_conversation_id());
                                                conversation.created_at = crate::domain::models::session_meta::now_unix();
                                                conversation.updated_at = crate::domain::models::session_meta::now_unix();
                                                conversation.last_response_at = None;
                                                conversation.usage = None;
                                                conversation.fork_source = None;
                                                save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                // U1: Clear status bar immediately after successful delete
                                                state.status = StatusState::Flash {
                                                    message: "All conversations deleted".to_string(),
                                                    remaining_ms: state.theme.timing.status_flash_ms,
                                                };
                                            }
                                        }
                                    }
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                }
                                InputAction::CancelDelete => {
                                    // Cancel the pending delete - U1: Clear status bar immediately
                                    state.pending_delete = None;
                                    // U1: Clear status bar immediately on cancel (don't restore previous status)
                                    state.status = StatusState::Idle;
                                    state.focus = if state.sidebar_visible {
                                        FocusState::Sidebar {
                                            panel: crate::domain::models::visual::PanelType::History,
                                            selected: state.sidebar_selected,
                                        }
                                    } else {
                                        FocusState::Input
                                    };
                                    state.needs_redraw = true;
                                }
                                // ── Fork Conversation (Story 4-3a) ──────────────
                                InputAction::ForkAtMessage => {
                                    // Guard: cannot fork while streaming
                                    if streaming.is_streaming {
                                        state.status = StatusState::Flash {
                                            message: "Cannot fork while streaming — wait for response to complete".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if conversation.messages.is_empty() {
                                        // Guard: no messages to fork
                                        state.status = StatusState::Flash {
                                            message: "Cannot fork: conversation has no messages".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if state.message_boundaries.is_empty() {
                                        // Guard: chat pane hasn't rendered yet
                                        state.status = StatusState::Flash {
                                            message: "Cannot fork: chat pane not ready".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else {
                                        // Determine which message is focused
                                        let fork_message_index = if state.auto_scroll {
                                            conversation.messages.len().saturating_sub(1)
                                        } else {
                                            let vp = state.terminal_height as usize;
                                            let max_off = state.total_content_height.saturating_sub(vp);
                                            let clamped = state.scroll_offset.min(max_off);
                                            let top_line = max_off.saturating_sub(clamped);
                                            match state.message_boundaries.binary_search(&top_line) {
                                                Ok(i) => i,
                                                Err(i) => i.saturating_sub(1),
                                            }
                                            .min(conversation.messages.len().saturating_sub(1))
                                        };
                                        state.pending_fork_index = Some(fork_message_index);
                                        state.focus = FocusState::Overlay(
                                            crate::domain::models::visual::OverlayType::Confirmation(
                                                crate::domain::models::visual::ConfirmationType::Fork,
                                            ),
                                        );
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::ForkConfirm => {
                                    // P5 dedup invariant (party-mode review 2026-04-12):
                                    // `.take()` drops pending_fork_index to None BEFORE the
                                    // storage await, so a second 'y' press that lands while
                                    // fork_at_checkpoint is in flight will be a no-op. The
                                    // event loop is single-threaded per iteration — crossterm
                                    // events queue behind the current select! branch and are
                                    // processed after we return. Do not move `.take()` below
                                    // the await without re-analyzing this invariant.
                                    if let Some(message_index) = state.pending_fork_index.take() {
                                        use crate::domain::models::checkpoint::CheckpointId;
                                        let checkpoint = CheckpointId(message_index as u64);
                                        let source_id = conversation.id.clone();
                                        let is_last = message_index == conversation.messages.len().saturating_sub(1);

                                        match storage.fork_at_checkpoint(&source_id, checkpoint).await {
                                            Ok(new_id) => {
                                                match storage.load_conversation(&new_id).await {
                                                    Ok(Some(forked_conv)) => {
                                                        // Save current tab state before creating new tab
                                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                        // Create new tab with the forked conversation
                                                        tab_manager.create_tab_with_conversation(forked_conv);
                                                        // Load the new active tab
                                                        let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                                        // Update session index
                                                        session_index.set_open(&conversation.id, true);
                                                        session_index.set_active(Some(&conversation.id));
                                                        state.sidebar_entry_count = session_index.len();
                                                        // Show fork hint in status bar
                                                        let hint_msg = if is_last {
                                                            "Forked at last message — both copies are now identical".to_string()
                                                        } else {
                                                            // Get the source conversation title from the fork_source.
                                                            // P4 (party-mode 2026-04-12): guard against empty/whitespace
                                                            // titles so the status doesn't render `Forked from "" at ...`.
                                                            let source_title = conversation.fork_source.as_ref()
                                                                .and_then(|fs| tab_manager.find_by_conversation(&fs.conversation_id))
                                                                .map(|t| t.conversation.title.clone())
                                                                .filter(|t| !t.trim().is_empty())
                                                                .unwrap_or_else(|| "(Untitled)".to_string());
                                                            format!(
                                                                "Forked from \"{}\" at message {}",
                                                                source_title,
                                                                message_index + 1
                                                            )
                                                        };
                                                        state.status = StatusState::Flash {
                                                            message: hint_msg,
                                                            remaining_ms: 5000,
                                                        };
                                                        state.focus = FocusState::Chat;
                                                    }
                                                    Ok(None) => {
                                                        tracing::error!("Forked conversation {} not found after creation", new_id);
                                                        state.focus = FocusState::Chat;
                                                    }
                                                    Err(e) => {
                                                        tracing::error!("Failed to load forked conversation: {}", e);
                                                        state.focus = FocusState::Chat;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("Fork failed: {}", e);
                                                state.status = StatusState::Flash {
                                                    message: format!("Fork failed: {}", e),
                                                    remaining_ms: 3000,
                                                };
                                                state.focus = FocusState::Chat;
                                            }
                                        }
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::ForkCancel => {
                                    state.pending_fork_index = None;
                                    state.focus = FocusState::Chat;
                                    state.needs_redraw = true;
                                }
                                // ── Rewind Conversation (Story 4-3b) ─────────────────
                                InputAction::RewindAtMessage => {
                                    // DF-018 guard: block if permission/question overlay is active
                                    if state.pending_permission.is_some() || state.ask_user_question.is_some() {
                                        state.status = StatusState::Flash {
                                            message: "Cannot rewind: a permission/question is pending \u{2014} answer it first".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if streaming.is_streaming {
                                        state.status = StatusState::Flash {
                                            message: "Cannot rewind while streaming \u{2014} wait for response to complete".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if conversation.messages.is_empty() {
                                        state.status = StatusState::Flash {
                                            message: "Cannot rewind: conversation has no messages".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if state.message_boundaries.is_empty() {
                                        state.status = StatusState::Flash {
                                            message: "Cannot rewind: chat pane not ready".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else {
                                        // MIRRORED FROM FORK: same message-targeting algorithm
                                        let target_message_index = if state.auto_scroll {
                                            conversation.messages.len().saturating_sub(1)
                                        } else {
                                            let vp = state.terminal_height as usize;
                                            let max_off = state.total_content_height.saturating_sub(vp);
                                            let clamped = state.scroll_offset.min(max_off);
                                            let top_line = max_off.saturating_sub(clamped);
                                            match state.message_boundaries.binary_search(&top_line) {
                                                Ok(i) => i,
                                                Err(i) => i.saturating_sub(1),
                                            }
                                            .min(conversation.messages.len().saturating_sub(1))
                                        };

                                        // Find target checkpoint covering this message
                                        let target_cp = resolve_checkpoint_for_message(
                                            &storage,
                                            &conversation.id,
                                            target_message_index,
                                        ).await;

                                        // Build preview (read-only, no mutations)
                                        let messages_to_remove = conversation.messages.len()
                                            .saturating_sub(target_message_index + 1);
                                        let files = storage
                                            .list_snapshot_files(&conversation.id, target_cp)
                                            .await
                                            .unwrap_or_default();
                                        let files_to_revert: Vec<crate::adapters::tui::state::RevertPreviewItem> = files
                                            .into_iter()
                                            .map(|(path, is_conflict)| {
                                                let display_path = path
                                                    .strip_prefix(&workspace_path)
                                                    .map(|p| p.to_string_lossy().to_string())
                                                    .unwrap_or_else(|_| path.to_string_lossy().to_string());
                                                crate::adapters::tui::state::RevertPreviewItem {
                                                    display_path,
                                                    conflict: is_conflict,
                                                }
                                            })
                                            .collect();

                                        state.pending_rewind_index = Some(target_message_index);
                                        state.rewind_preview = Some(crate::adapters::tui::state::RewindPreview {
                                            target_message_index,
                                            messages_to_remove,
                                            files_to_revert,
                                        });
                                        state.focus = FocusState::Overlay(
                                            crate::domain::models::visual::OverlayType::Confirmation(
                                                crate::domain::models::visual::ConfirmationType::Rewind,
                                            ),
                                        );
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::RewindConfirm => {
                                    // P5 dedup invariant: .take() before await prevents double-confirm
                                    if let Some(target_msg_idx) = state.pending_rewind_index.take() {
                                        state.rewind_preview = None;

                                        // Resolve the checkpoint floor for FILE snapshots only.
                                        // Message truncation uses `target_msg_idx` directly via
                                        // `truncate_conversation` so the user's selection is
                                        // honored (bug 1b fix). The checkpoint id returned here
                                        // governs which snapshot files are eligible for revert
                                        // (every cp_id > target_cp gets applied in reverse order).
                                        let target_cp = resolve_checkpoint_for_message(
                                            &storage,
                                            &conversation.id,
                                            target_msg_idx,
                                        ).await;

                                        // 1. Truncate conversation to the user's selected message
                                        //    index (pure message-level; no checkpoint dependency).
                                        match storage.truncate_conversation(&conversation.id, target_msg_idx).await {
                                            Ok(truncated) => {
                                                // 2. Revert files
                                                let reverted = storage
                                                    .revert_file_snapshots(&conversation.id, target_cp)
                                                    .await
                                                    .unwrap_or_default();
                                                let restored = reverted
                                                    .iter()
                                                    .filter(|r| matches!(r.status, crate::domain::models::RevertStatus::Restored))
                                                    .count();
                                                let conflicts = reverted
                                                    .iter()
                                                    .filter(|r| matches!(r.status, crate::domain::models::RevertStatus::Conflict { .. }))
                                                    .count();

                                                // 3. Update active-tab proxy (AC3 step 3)
                                                save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                tab_manager.active_tab_mut().conversation = truncated;
                                                let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);

                                                // 4. DF-005: invalidate height cache entries for removed messages.
                                                // load_active_tab already called invalidate_all(); truncate_from
                                                // is a no-op here but satisfies the API contract and unit tests.
                                                state.height_cache.truncate_from(target_msg_idx + 1);

                                                // 5. Cancel in-flight permission/question + drain queue (AC3 step 5)
                                                // Dropping response_tx closes the channel → turn gets Err
                                                drop(state.pending_permission.take());
                                                drop(state.ask_user_question.take());
                                                drop(state.question_response_tx.take());
                                                while turn_queue.dequeue().is_some() {}
                                                // Abort active turn
                                                if let Some(handle) = _active_turn.take() {
                                                    handle.abort();
                                                }

                                                // 6. Status hint (5000ms — completed action, Status Timeout Rule)
                                                let status_msg = if conflicts > 0 {
                                                    format!(
                                                        "\u{2139} Rewound to message {}. Reverted {} files. \u{26a0} {} files skipped (modified externally).",
                                                        target_msg_idx + 1, restored, conflicts
                                                    )
                                                } else {
                                                    format!(
                                                        "\u{2139} Rewound to message {}. Reverted {} files.",
                                                        target_msg_idx + 1, restored
                                                    )
                                                };
                                                state.status = StatusState::Flash {
                                                    message: status_msg,
                                                    remaining_ms: 5000,
                                                };
                                            }
                                            Err(e) => {
                                                tracing::error!("revert_to_checkpoint failed: {}", e);
                                                state.status = StatusState::Flash {
                                                    message: format!("Rewind failed: {}", e),
                                                    remaining_ms: 3000,
                                                };
                                            }
                                        }
                                        state.focus = FocusState::Chat;
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::RewindCancel => {
                                    state.pending_rewind_index = None;
                                    state.rewind_preview = None;
                                    state.focus = FocusState::Chat;
                                    state.needs_redraw = true;
                                }
                                InputAction::RewindForkInstead => {
                                    // AC4: rewind → fork-instead path; mirrors ForkConfirm body.
                                    state.rewind_preview = None;
                                    if let Some(message_index) = state.pending_rewind_index.take() {
                                        use crate::domain::models::checkpoint::CheckpointId;
                                        let checkpoint = CheckpointId(message_index as u64);
                                        let source_id = conversation.id.clone();
                                        let is_last = message_index == conversation.messages.len().saturating_sub(1);

                                        match storage.fork_at_checkpoint(&source_id, checkpoint).await {
                                            Ok(new_id) => {
                                                match storage.load_conversation(&new_id).await {
                                                    Ok(Some(forked_conv)) => {
                                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                        tab_manager.create_tab_with_conversation(forked_conv);
                                                        let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                                        session_index.set_open(&conversation.id, true);
                                                        session_index.set_active(Some(&conversation.id));
                                                        state.sidebar_entry_count = session_index.len();
                                                        let hint_msg = if is_last {
                                                            "Forked at last message \u{2014} both copies are now identical".to_string()
                                                        } else {
                                                            let source_title = conversation.fork_source.as_ref()
                                                                .and_then(|fs| tab_manager.find_by_conversation(&fs.conversation_id))
                                                                .map(|t| t.conversation.title.clone())
                                                                .filter(|t| !t.trim().is_empty())
                                                                .unwrap_or_else(|| "(Untitled)".to_string());
                                                            format!(
                                                                "Forked from \"{}\" at message {}",
                                                                source_title,
                                                                message_index + 1
                                                            )
                                                        };
                                                        state.status = StatusState::Flash {
                                                            message: hint_msg,
                                                            remaining_ms: 5000,
                                                        };
                                                        state.focus = FocusState::Chat;
                                                    }
                                                    Ok(None) => {
                                                        tracing::error!("Forked conversation {} not found after creation", new_id);
                                                        state.focus = FocusState::Chat;
                                                    }
                                                    Err(e) => {
                                                        tracing::error!("Failed to load forked conversation: {}", e);
                                                        state.focus = FocusState::Chat;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("Fork (from rewind) failed: {}", e);
                                                state.status = StatusState::Flash {
                                                    message: format!("Fork failed: {}", e),
                                                    remaining_ms: 3000,
                                                };
                                                state.focus = FocusState::Chat;
                                            }
                                        }
                                        state.needs_redraw = true;
                                    } else {
                                        state.focus = FocusState::Chat;
                                        state.needs_redraw = true;
                                    }
                                }
                                // ── Story 4-4: Within-Conversation Search ──────────────
                                InputAction::OpenSearch => {
                                    // app.rs::handle_special_key already set focus
                                    // and activated search_state. Event loop just
                                    // needs to trigger the initial (empty) find_matches
                                    // so the counter renders 0/0 correctly and mark redraw.
                                    state.needs_redraw = true;
                                }
                                InputAction::CloseSearch => {
                                    let prior = state
                                        .search_state
                                        .prior_focus
                                        .take()
                                        .unwrap_or(FocusState::Chat);
                                    state.search_state =
                                        crate::adapters::tui::state::SearchState::new();
                                    state.focus = prior;
                                    state.needs_redraw = true;
                                }
                                InputAction::SearchQueryChanged
                                | InputAction::SearchClear
                                | InputAction::SearchReturnToTyping => {
                                    apply_search_rescan(&conversation, &mut state);
                                }
                                InputAction::SearchCommit => {
                                    if !state.search_state.matches.is_empty() {
                                        state.search_state.substate =
                                            crate::adapters::tui::state::SearchSubstate::Navigating;
                                    } else {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "No matches found".to_string(),
                                            remaining_ms: 1200,
                                        };
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::SearchNext => {
                                    apply_search_navigate(&mut state, 1);
                                }
                                InputAction::SearchPrev => {
                                    apply_search_navigate(&mut state, -1);
                                }
                                // ── Story 4-4: Bookmarks ──────────────────────────────
                                InputAction::ToggleBookmark => {
                                    // Await any in-flight conversation save before mutating
                                    // meta.json to serialize all writes to the same file
                                    // (Dev Notes § Bookmark Persistence — Failure Mode 1).
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    apply_bookmark_toggle(
                                        &mut tab_manager,
                                        &mut state,
                                        &conversation,
                                        &fs_storage,
                                    ).await;
                                }
                                InputAction::OpenBookmarkList => {
                                    apply_open_bookmark_list(&tab_manager, &mut state);
                                }
                                InputAction::JumpToBookmark => {
                                    apply_jump_bookmark(&tab_manager, &mut state);
                                }
                                InputAction::DeleteBookmark => {
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    apply_delete_bookmark(
                                        &mut tab_manager,
                                        &mut state,
                                        &conversation,
                                        &fs_storage,
                                    ).await;
                                }
                                InputAction::UndoBookmarkDelete => {
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    apply_undo_bookmark_delete(
                                        &mut tab_manager,
                                        &mut state,
                                        &conversation,
                                        &fs_storage,
                                    ).await;
                                }
                                InputAction::CloseBookmarkList => {
                                    state.focus = FocusState::Chat;
                                    state.bookmark_list_selected = 0;
                                    state.needs_redraw = true;
                                }
                                // ── Story 4-4: Cross-Conversation Search ──────────────
                                InputAction::OpenCrossSearch => {
                                    state.needs_redraw = true;
                                }
                                InputAction::CrossSearchQueryChanged => {
                                    // Story 4-4 AC5 — delegate the guard to
                                    // `apply_cross_search_query_change` so tests exercise the
                                    // same code path (third-audit Fix R4). If the helper
                                    // returns Spawn, kick off the async scan; Cleared is a
                                    // no-op aside from the needs_redraw below.
                                    match apply_cross_search_query_change(&mut state) {
                                        CrossSearchScanAction::Spawn { query } => {
                                            let storage_ref = storage.clone();
                                            let index_clone = session_index.clone();
                                            let tx = domain_tx.clone();
                                            tokio::spawn(async move {
                                                use crate::domain::services::cross_search::{
                                                    CrossSearchBudget, run_cross_search,
                                                };
                                                let outcome = run_cross_search(
                                                    &*storage_ref,
                                                    &index_clone,
                                                    &query,
                                                    CrossSearchBudget::default(),
                                                )
                                                .await;
                                                let _ = tx.send(AppEvent::CrossSearchResultsReady {
                                                    query,
                                                    results: outcome.results,
                                                    truncated_by_count: outcome.truncated_by_count,
                                                    truncated_by_time: outcome.truncated_by_time,
                                                });
                                            });
                                        }
                                        CrossSearchScanAction::Cleared => {}
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::OpenCrossSearchResult => {
                                    apply_open_cross_search_result(
                                        &mut tab_manager,
                                        &mut conversation,
                                        &mut streaming,
                                        &mut session_manager,
                                        &mut state,
                                        &mut turn_queue,
                                        &*storage,
                                        &fs_storage,
                                        &mut session_index,
                                        &domain_tx,
                                    )
                                    .await;
                                }
                                InputAction::CloseCrossSearch => {
                                    state.cross_search =
                                        crate::adapters::tui::state::CrossSearchState::new();
                                    state.focus = FocusState::Sidebar {
                                        panel: crate::domain::models::visual::PanelType::History,
                                        selected: state.sidebar_selected,
                                    };
                                    state.needs_redraw = true;
                                }
                                InputAction::ConfirmExportOverwrite => {
                                    // Third-audit Fix R6 — delegate to the `pub` helper so
                                    // integration tests exercise the same code path as the
                                    // event loop.
                                    apply_confirm_export_overwrite(&mut state).await;
                                }
                                InputAction::CancelExportOverwrite => {
                                    apply_cancel_export_overwrite(&mut state);
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
                                crate::domain::models::session_meta::now_unix(),
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

                                    // Update sidebar index on turn complete
                                    session_index.touch(
                                        &conversation.id,
                                        if conversation.title.is_empty() { None } else { Some(conversation.title.clone()) },
                                        Some(conversation.messages.len()),
                                    );
                                    state.sidebar_entry_count = session_index.len();

                                    // Persist conversation on turn complete
                                    if persist {
                                        let now = crate::domain::models::session_meta::now_unix();
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
                                            &fs_storage,
                                            &storage,
                                        );
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
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
                                crate::domain::models::session_meta::now_unix(),
                            );
                            if let ChunkAction::TurnComplete { persist, trigger_title_generation } = action {
                                // apply_chunk already set tab.streaming.is_streaming = false
                                if persist {
                                    let now = crate::domain::models::session_meta::now_unix();
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
                        }
                    }
                    AppEvent::SystemNotice { conversation_id: notice_conv_id, level, message: msg } => {
                        // Route by conversation_id: None = global (always active tab),
                        // Some(id) = only affects that conversation's tab.
                        let is_active_tab = notice_conv_id
                            .as_deref()
                            .is_none_or(|id| id == conversation.id);

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
                            // DF-018 inverse guard (AC6, Story 4-3b): if the rewind overlay is active
                            // for this conversation, the permission request is for a turn that is about
                            // to be truncated out of existence — drop it silently.
                            if matches!(
                                state.focus,
                                FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind))
                            ) {
                                tracing::debug!(
                                    "Dropping permission request for conversation {} — rewind in progress",
                                    conversation_id
                                );
                                // Drop response_tx — sender side closed, turn gets Err on the other end
                            } else {
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
                            }
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
                            &fs_storage,
                            &storage,
                        );
                        match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
                            Ok(()) => state.needs_redraw = false,
                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                        }
                    }
                    AppEvent::TitleGenerated { conversation_id, title } => {
                        // Update sidebar index with new title and reorder to front
                        session_index.touch(&conversation_id, Some(title.clone()), None);
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
                            // DF-018 inverse guard (AC6, Story 4-3b): rewind overlay active →
                            // drop the question request silently (its turn will be truncated).
                            if matches!(
                                state.focus,
                                FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind))
                            ) {
                                tracing::debug!(
                                    "Dropping AskUserQuestion for conversation {} — rewind in progress",
                                    conversation_id
                                );
                                // Drop response_tx — turn gets RecvError on the other end
                            } else {
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
                    AppEvent::CrossSearchResultsReady {
                        query,
                        results,
                        truncated_by_count,
                        truncated_by_time,
                    } => {
                        // Story 4-4 AC5 stale-result guard — delegated to the
                        // `apply_cross_search_results` helper so the event loop
                        // and tests exercise the same code path
                        // (third-audit Fix R4).
                        let outcome = apply_cross_search_results(
                            &mut state,
                            query,
                            results,
                            truncated_by_count,
                            truncated_by_time,
                        );
                        if matches!(outcome, CrossSearchResultsOutcome::Applied) {
                            state.needs_redraw = true;
                        }
                    }
                    AppEvent::PeekHighlightExpired { tab_id: _ } => {
                        // Story 4-4 AC6 — peek highlight window has elapsed.
                        // Clear the peek on the active tab's search state.
                        // `tab_id` is passed for future multi-tab routing but
                        // currently all peeks are applied to the active tab.
                        state.search_state.peek_highlight = None;
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
                    match render(terminal, &mut state, &conversation, &streaming, &config.model, security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
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
        conversation.updated_at = crate::domain::models::session_meta::now_unix();
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
            bg_conv.updated_at = crate::domain::models::session_meta::now_unix();
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
/// Re-scan the active conversation for search matches and apply the calm-jump
/// rule (Story 4-4 AC2 amendment). Called from event-loop arms that mutate
/// `state.search_state.query` (keystrokes in Typing sub-state, Ctrl+U clear,
/// and return-from-Navigating transitions).
///
/// Calm-jump rule:
///   1. Jump to match 0 only if match count transitioned 0 → ≥1
///   2. Jump to match 0 only if the previously focused match no longer exists
///   3. Otherwise preserve the viewport (no yo-yo)
fn apply_search_rescan(conversation: &Conversation, state: &mut TuiState) {
    use crate::adapters::tui::widgets::chat_pane;
    use crate::domain::services::search::find_matches;

    // Story 4-4 Task 3.3 — debounce guard: skip the scan if the last scan
    // happened less than 30 ms ago AND the query length did not change.
    // This avoids burning CPU on held-key repeats without losing accuracy on
    // normal typing (where length changes every keystroke).
    let prev_query_len = state.search_state.last_query_len;
    let cur_query_len = state.search_state.query.chars().count();
    if let Some(last) = state.search_state.last_search_instant {
        if last.elapsed() < std::time::Duration::from_millis(30) && prev_query_len == cur_query_len
        {
            return;
        }
    }
    state.search_state.last_query_len = cur_query_len;

    let prev_focused = state
        .search_state
        .matches
        .get(state.search_state.focused_match_index)
        .cloned();
    let prev_was_empty = state.search_state.matches.is_empty();

    let new_matches = find_matches(conversation, &state.search_state.query);
    let new_is_empty = new_matches.is_empty();
    let prev_focused_still_valid = match &prev_focused {
        Some(f) => new_matches.contains(f),
        None => false,
    };
    state.search_state.matches = new_matches;
    state.search_state.last_search_instant = Some(std::time::Instant::now());

    let should_jump = (prev_was_empty && !new_is_empty) || !prev_focused_still_valid;
    if should_jump && !state.search_state.matches.is_empty() {
        state.search_state.focused_match_index = 0;
        let target_msg = state.search_state.matches[0].message_index;
        state.scroll_offset = chat_pane::find_scroll_offset_for_message(
            target_msg,
            &state.message_boundaries,
            state.total_content_height,
            state.terminal_height as usize,
        );
        state.auto_scroll = state.scroll_offset == 0;
    }
    state.needs_redraw = true;
}

/// Action returned by `apply_cross_search_query_change` so the event loop
/// knows whether to spawn an async scan task or skip scanning entirely
/// (Story 4-4 AC5 + third-audit Fix R4).
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossSearchScanAction {
    /// Query is ≥ 2 chars — the event loop should spawn `run_cross_search`
    /// with the carried query string.
    Spawn { query: String },
    /// Query is < 2 chars — `state.cross_search.results` has already been
    /// cleared in-place by this helper. No scan should be spawned.
    Cleared,
}

/// Guard helper for the `CrossSearchQueryChanged` input action. Encapsulates
/// the "≥ 2 chars → scan, else clear" decision so the event loop and the
/// tests call the SAME logic — no more tautological copies of the guard
/// (third-audit Fix R4).
#[doc(hidden)]
pub fn apply_cross_search_query_change(state: &mut TuiState) -> CrossSearchScanAction {
    if state.cross_search.query.chars().count() >= 2 {
        state.cross_search.running = true;
        CrossSearchScanAction::Spawn {
            query: state.cross_search.query.clone(),
        }
    } else {
        state.cross_search.results.clear();
        state.cross_search.selected = 0;
        state.cross_search.truncated_by_count = false;
        state.cross_search.truncated_by_time = false;
        state.cross_search.running = false;
        CrossSearchScanAction::Cleared
    }
}

/// Story 4-4 AC12 + third-audit Fix R6 — commit the pre-staged export
/// content when the user confirms the overwrite overlay with `y`.
///
/// Atomic write: tmp → fsync → rename, all routed through
/// `tokio::task::spawn_blocking` so the event loop is never held on disk
/// latency. Takes `state.pending_export` and clears it regardless of
/// success/failure. Sets a success or error flash and returns focus to Chat.
///
/// Extracted as `pub` so integration tests can exercise the full
/// confirmation path against a real tempdir workspace.
#[doc(hidden)]
pub async fn apply_confirm_export_overwrite(state: &mut TuiState) {
    if let Some((target_path, content)) = state.pending_export.take() {
        let target_for_msg = target_path.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let tmp_path = target_path.with_extension("md.tmp");
            let res: std::io::Result<()> = (|| {
                let mut f = std::fs::File::create(&tmp_path)?;
                f.write_all(content.as_bytes())?;
                f.sync_all()?;
                drop(f);
                std::fs::rename(&tmp_path, &target_path)?;
                Ok(())
            })();
            if res.is_err() {
                let _ = std::fs::remove_file(&tmp_path);
            }
            res
        })
        .await;
        match write_result {
            Ok(Ok(())) => {
                state.status = StatusState::Flash {
                    message: format!("Overwrote {}", target_for_msg.display()),
                    remaining_ms: 3000,
                };
            }
            Ok(Err(e)) => {
                state.status = StatusState::Flash {
                    message: format!("Export failed: {}", e),
                    remaining_ms: 3000,
                };
            }
            Err(join_err) => {
                state.status = StatusState::Flash {
                    message: format!("Export failed: {}", join_err),
                    remaining_ms: 3000,
                };
            }
        }
    }
    state.focus = FocusState::Chat;
    state.needs_redraw = true;
}

/// Story 4-4 AC12 + third-audit Fix R6 — drop the pre-staged export content
/// when the user dismisses the overwrite overlay with `n` or `Esc`.
///
/// Clears `state.pending_export`, sets a "cancelled" flash, and returns
/// focus to Chat. The original file on disk is never touched by this path.
#[doc(hidden)]
pub fn apply_cancel_export_overwrite(state: &mut TuiState) {
    state.pending_export = None;
    state.status = StatusState::Flash {
        message: "Export cancelled".to_string(),
        remaining_ms: 2000,
    };
    state.focus = FocusState::Chat;
    state.needs_redraw = true;
}

/// Outcome returned by `apply_cross_search_results` so callers know whether
/// the incoming scan results were applied or dropped as stale (Story 4-4 AC5
/// stale-result guard + third-audit Fix R4).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossSearchResultsOutcome {
    /// Results matched the current query and were written into state.
    Applied,
    /// Results were for an older query; discarded silently.
    DiscardedStale,
}

/// Handler helper for the `AppEvent::CrossSearchResultsReady` event.
/// Applies the stale-result guard: if the incoming `query` still matches
/// `state.cross_search.query`, the results are written into state;
/// otherwise they're dropped.
#[doc(hidden)]
pub fn apply_cross_search_results(
    state: &mut TuiState,
    query: String,
    results: Vec<crate::domain::services::cross_search::CrossSearchResult>,
    truncated_by_count: bool,
    truncated_by_time: bool,
) -> CrossSearchResultsOutcome {
    if state.cross_search.query != query {
        return CrossSearchResultsOutcome::DiscardedStale;
    }
    state.cross_search.results = results;
    state.cross_search.truncated_by_count = truncated_by_count;
    state.cross_search.truncated_by_time = truncated_by_time;
    state.cross_search.running = false;
    if state.cross_search.results.is_empty() {
        state.cross_search.selected = 0;
    } else {
        state.cross_search.selected = state
            .cross_search
            .selected
            .min(state.cross_search.results.len() - 1);
    }
    CrossSearchResultsOutcome::Applied
}

/// Advance or reverse the focused search match, wrapping at boundaries.
/// Emits a "Wrapped to top" / "Wrapped to bottom" flash (800 ms) when the
/// index wraps past 0 or `matches.len() - 1` (Story 4-4 AC3 amendment Fix 3).
///
/// `delta`: +1 for `n` (next), -1 for `N` (previous).
fn apply_search_navigate(state: &mut TuiState, delta: i32) {
    use crate::adapters::tui::widgets::chat_pane;
    if state.search_state.matches.is_empty() {
        state.needs_redraw = true;
        return;
    }
    let len = state.search_state.matches.len();
    let prev = state.search_state.focused_match_index;
    let new_idx = if delta > 0 {
        (prev + 1) % len
    } else {
        (prev + len - 1) % len
    };
    state.search_state.focused_match_index = new_idx;
    let target_msg = state.search_state.matches[new_idx].message_index;
    state.scroll_offset = chat_pane::find_scroll_offset_for_message(
        target_msg,
        &state.message_boundaries,
        state.total_content_height,
        state.terminal_height as usize,
    );
    state.auto_scroll = state.scroll_offset == 0;

    // Wrap-around flash (only fires when len > 1 — a single match never wraps visibly).
    if len > 1 {
        let wrapped_forward = delta > 0 && prev == len - 1 && new_idx == 0;
        let wrapped_backward = delta < 0 && prev == 0 && new_idx == len - 1;
        if wrapped_forward {
            state.status_before_flash = Some(state.status.clone());
            state.status = StatusState::Flash {
                message: "Wrapped to top".to_string(),
                remaining_ms: 800,
            };
        } else if wrapped_backward {
            state.status_before_flash = Some(state.status.clone());
            state.status = StatusState::Flash {
                message: "Wrapped to bottom".to_string(),
                remaining_ms: 800,
            };
        }
    }
    state.needs_redraw = true;
}

/// Story 4-4 AC8: toggle a bookmark on the currently focused message.
///
/// Resolves the target via `find_message_index_from_scroll_offset`, mutates
/// the in-memory mirror on the active tab's `session_meta.bookmarks`, then
/// atomically persists `meta.json` via `save_session_meta`. If the disk save
/// fails, the in-memory change is rolled back and an error flash is shown
/// (AC8 optimistic-UI contract).
///
/// Rejects tool-call / tool-result messages with a flash — bookmarks only
/// apply to user/assistant messages per Dev Notes § Bookmarkable Message
/// Types.
async fn apply_bookmark_toggle(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
) {
    use crate::adapters::tui::widgets::chat_pane::find_message_index_from_scroll_offset;

    if conversation.messages.is_empty() {
        state.status = StatusState::Flash {
            message: "No messages to bookmark".to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return;
    }
    if state.message_boundaries.is_empty() {
        state.status = StatusState::Flash {
            message: "Chat pane not ready — try again after first render".to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return;
    }

    let target_idx = find_message_index_from_scroll_offset(
        state.auto_scroll,
        state.scroll_offset,
        &state.message_boundaries,
        state.total_content_height,
        state.terminal_height as usize,
        conversation.messages.len(),
    );

    // Guard: bookmark only user/assistant messages. Tool messages have
    // structural first lines; prefixing them with `» ` looks like an error.
    use crate::domain::models::MessageRole;
    let role = conversation.messages[target_idx].role;
    if !matches!(role, MessageRole::User | MessageRole::Assistant) {
        state.status = StatusState::Flash {
            message: "Cannot bookmark tool message — target a user or assistant message"
                .to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return;
    }

    // Clone-mutate-save pattern for rollback safety.
    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    let was_bookmarked = new_meta.bookmarks.binary_search(&target_idx).is_ok();
    if was_bookmarked {
        new_meta.bookmarks.retain(|&i| i != target_idx);
    } else {
        match new_meta.bookmarks.binary_search(&target_idx) {
            Ok(_) => {} // already there, should not happen given we checked above
            Err(pos) => new_meta.bookmarks.insert(pos, target_idx),
        }
    }

    // Optimistic UI: update in-memory first.
    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();

    // Persist to disk. Rollback on failure.
    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            // Third-audit Fix R1: flash indices are 0-based per AC10 — match
            // the bookmark list display and the internal Vec<usize> storage.
            let msg = if was_bookmarked {
                format!("Bookmark removed (msg {})", target_idx)
            } else {
                format!("Bookmark added (msg {})", target_idx)
            };
            state.status = StatusState::Flash {
                message: msg,
                remaining_ms: 2000,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to save bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
}

/// Story 4-4 AC10: open the bookmark list panel. Flashes a teaching message
/// when the active tab has no bookmarks (reviewer Fix 10 — warmer copy).
fn apply_open_bookmark_list(tab_manager: &TabManager, state: &mut TuiState) {
    let bookmarks = &tab_manager.active_tab().session_meta.bookmarks;
    if bookmarks.is_empty() {
        state.status = StatusState::Flash {
            message: "No bookmarks in this conversation — press 'm' on a message to add one"
                .to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return;
    }
    state.focus = FocusState::Overlay(crate::domain::models::visual::OverlayType::BookmarkList);
    // AC10 amendment (party-mode Fix 30): reset selection to 0 on every open
    // so a stale index from a previous session doesn't survive the close.
    state.bookmark_list_selected = 0;
    // AC10 amendment (party-mode Fix 18): mirror bookmark count onto state
    // for the key-handler's upper-bound clamp (j / Down).
    state.bookmark_list_count = bookmarks.len();
    state.needs_redraw = true;
}

/// Story 4-4 AC10: jump to the currently selected bookmark.
fn apply_jump_bookmark(tab_manager: &TabManager, state: &mut TuiState) {
    use crate::adapters::tui::widgets::chat_pane;
    let bookmarks = &tab_manager.active_tab().session_meta.bookmarks;
    if bookmarks.is_empty() {
        state.focus = FocusState::Chat;
        state.needs_redraw = true;
        return;
    }
    let sel = state.bookmark_list_selected.min(bookmarks.len() - 1);
    let target_msg = bookmarks[sel];
    state.scroll_offset = chat_pane::find_scroll_offset_for_message(
        target_msg,
        &state.message_boundaries,
        state.total_content_height,
        state.terminal_height as usize,
    );
    state.auto_scroll = state.scroll_offset == 0;
    state.focus = FocusState::Chat;
    state.bookmark_list_selected = 0;
    state.needs_redraw = true;
}

/// Story 4-4 AC10: delete the currently selected bookmark, stashing it in
/// the undo buffer for 5 s (synthesis of Sally #7 + Reviewer Fix 8).
async fn apply_delete_bookmark(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
) {
    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    if new_meta.bookmarks.is_empty() {
        return;
    }
    let sel = state
        .bookmark_list_selected
        .min(new_meta.bookmarks.len() - 1);
    let removed_idx = new_meta.bookmarks.remove(sel);

    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();

    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            // Stash in undo buffer (single-entry, 5 s window).
            state.bookmark_undo_buffer = Some((removed_idx, std::time::Instant::now()));
            // Keep the mirror count in sync for the key handler's clamp.
            state.bookmark_list_count = new_meta.bookmarks.len();
            // Clamp selection to the new list length.
            if new_meta.bookmarks.is_empty() {
                state.bookmark_list_selected = 0;
                state.focus = FocusState::Chat;
            } else {
                state.bookmark_list_selected = state
                    .bookmark_list_selected
                    .min(new_meta.bookmarks.len() - 1);
            }
            state.status = StatusState::Flash {
                message: format!("Bookmark removed (msg {}) — press u to undo", removed_idx),
                remaining_ms: 2000,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to delete bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
}

/// Story 4-4 AC10: undo the most recent bookmark delete (if within 5 s).
async fn apply_undo_bookmark_delete(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
) {
    let Some((idx, when)) = state.bookmark_undo_buffer else {
        return; // silent no-op
    };
    if when.elapsed() > std::time::Duration::from_secs(5) {
        state.bookmark_undo_buffer = None;
        return; // silent no-op (expired)
    }

    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    // Re-insert sorted.
    match new_meta.bookmarks.binary_search(&idx) {
        Ok(_) => {
            // Already present — nothing to do but clear the buffer.
            state.bookmark_undo_buffer = None;
            return;
        }
        Err(pos) => new_meta.bookmarks.insert(pos, idx),
    }

    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();
    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            state.bookmark_undo_buffer = None;
            state.status = StatusState::Flash {
                message: format!("Bookmark restored (msg {})", idx),
                remaining_ms: 1500,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to restore bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
}

/// Story 4-4 AC11/AC12: export the active conversation to markdown.
///
/// Dual-mode semantics:
/// - `/export` (no arg) → **auto-number mode**: write to
///   `.rustain/exports/<slug>.md`, incrementing a `-N` suffix until a free
///   name is found. Never prompts, never overwrites.
/// - `/export <filename>` → **explicit-path mode**: write to the named path
///   relative to `.rustain/exports/`. On collision, opens the Tier-1
///   `ExportOverwrite` confirmation overlay with the pre-rendered content
///   stashed in `state.pending_export` for a stable snapshot at confirm time.
///
/// Runs async. All blocking filesystem I/O is routed through
/// `tokio::task::spawn_blocking` so the TUI event loop is never held up by
/// disk latency.
///
/// Path-traversal hardening (second-audit Fix 27): `/export <name>` rejects
/// absolute paths and any relative path that contains `..` components, and
/// additionally verifies the canonicalized parent stays within the
/// `.rustain/exports/` directory. Supersedes the original Dev Notes wording
/// that allowed absolute paths.
///
/// Atomic write: `.tmp` → `fsync` → `rename` per Dev Notes § Atomic Write
/// Pattern. Keeps crash-safety guarantees for exported files.
///
/// **Visibility:** marked `pub` for integration test access — the tests in
/// `tests/e2e_export_filesystem.rs` need to exercise the full flow with a
/// real tempdir workspace. Not intended for external callers.
#[doc(hidden)]
pub async fn apply_export_command(
    arg: Option<&str>,
    conversation: &Conversation,
    meta: &crate::domain::models::SessionMeta,
    workspace_path: &std::path::Path,
    state: &mut TuiState,
) {
    use crate::domain::services::export::{render_conversation_markdown, slugify};

    let exports_dir = workspace_path.join(".rustain").join("exports");
    // Create the exports dir on a blocking task.
    {
        let exports_dir = exports_dir.clone();
        let create_result =
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(&exports_dir)).await;
        match create_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                state.status = StatusState::Flash {
                    message: format!("Export failed: cannot create exports dir: {}", e),
                    remaining_ms: 3000,
                };
                state.needs_redraw = true;
                return;
            }
            Err(join_err) => {
                state.status = StatusState::Flash {
                    message: format!("Export failed: {}", join_err),
                    remaining_ms: 3000,
                };
                state.needs_redraw = true;
                return;
            }
        }
    }

    // Canonicalize the exports_dir for the path-traversal check.
    let canonical_exports = match tokio::task::spawn_blocking({
        let exports_dir = exports_dir.clone();
        move || std::fs::canonicalize(&exports_dir)
    })
    .await
    {
        Ok(Ok(p)) => p,
        _ => exports_dir.clone(),
    };

    // Resolve the target path.
    let target_path: std::path::PathBuf = match arg {
        None => {
            // Auto-number mode — browser-style. Loop unbounded; if the user
            // has 1000+ exports they hit them in practice, that's their
            // problem per Story 4-4 Dev Notes § Export Invocation Modes.
            let base_slug = if conversation.title.is_empty() {
                format!(
                    "conversation-{}",
                    &conversation.id[..8.min(conversation.id.len())]
                )
            } else {
                slugify(&conversation.title)
            };
            let candidate = exports_dir.join(format!("{}.md", base_slug));
            find_available_numbered_path(candidate, &exports_dir, &base_slug).await
        }
        Some(name) => {
            // Explicit-path mode — Phase 3 security: reject absolute paths
            // and path-traversal attempts that escape the exports dir.
            let raw = std::path::PathBuf::from(name);
            if raw.is_absolute() {
                state.status = StatusState::Flash {
                    message:
                        "Export failed: absolute paths are not allowed (use a name relative to .rustain/exports/)"
                            .to_string(),
                    remaining_ms: 3500,
                };
                state.needs_redraw = true;
                return;
            }
            // Reject paths containing `..` components outright — simpler and
            // stronger than canonicalize-then-compare because non-existent
            // files cannot be canonicalized.
            if raw
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                state.status = StatusState::Flash {
                    message: "Export failed: path contains '..' — not allowed".to_string(),
                    remaining_ms: 3500,
                };
                state.needs_redraw = true;
                return;
            }
            let candidate = exports_dir.join(&raw);
            // Defense-in-depth: canonicalize the parent and verify containment.
            // If the parent doesn't exist yet, that's acceptable — we rejected
            // `..` above, so the candidate cannot escape.
            if let Some(parent) = candidate.parent() {
                if let Ok(Ok(canonical_parent)) = tokio::task::spawn_blocking({
                    let parent = parent.to_path_buf();
                    move || std::fs::canonicalize(&parent)
                })
                .await
                {
                    if !canonical_parent.starts_with(&canonical_exports) {
                        state.status = StatusState::Flash {
                            message: "Export failed: target escapes .rustain/exports/".to_string(),
                            remaining_ms: 3500,
                        };
                        state.needs_redraw = true;
                        return;
                    }
                }
            }
            candidate
        }
    };

    // Render content before checking for collisions so the overlay captures a
    // stable snapshot (avoids races with the conversation mutating between
    // the `y` press and the write).
    let now = crate::domain::models::session_meta::now_unix();
    let content = render_conversation_markdown(conversation, meta, now);

    // Check collision for explicit-path mode (arg = Some) — open the Tier-1
    // ExportOverwrite confirmation overlay instead of flashing an error.
    let target_exists = tokio::task::spawn_blocking({
        let target_path = target_path.clone();
        move || target_path.exists()
    })
    .await
    .unwrap_or(false);
    if arg.is_some() && target_exists {
        state.pending_export = Some((target_path.clone(), content));
        state.focus =
            FocusState::Overlay(crate::domain::models::visual::OverlayType::Confirmation(
                crate::domain::models::visual::ConfirmationType::ExportOverwrite(target_path),
            ));
        state.needs_redraw = true;
        return;
    }

    // Atomic write on a blocking thread.
    let workspace_path_owned = workspace_path.to_path_buf();
    let target_path_owned = target_path.clone();
    let write_result = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let tmp_path = target_path_owned.with_extension("md.tmp");
        let write: std::io::Result<()> = (|| {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp_path, &target_path_owned)?;
            Ok(())
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        write
    })
    .await;

    match write_result {
        Ok(Ok(())) => {
            let display_path = target_path
                .strip_prefix(&workspace_path_owned)
                .unwrap_or(&target_path);
            state.status = StatusState::Flash {
                message: format!("Exported to {}", display_path.display()),
                remaining_ms: 3000,
            };
        }
        Ok(Err(e)) => {
            state.status = StatusState::Flash {
                message: format!("Export failed: {}", e),
                remaining_ms: 3000,
            };
        }
        Err(join_err) => {
            state.status = StatusState::Flash {
                message: format!("Export failed: {}", join_err),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
}

/// Auto-number helper: walk `<slug>.md`, `<slug>-2.md`, ... until we find a
/// filename that doesn't exist. Unbounded per spec — in the absurd case of
/// 10000+ exports the user will notice before we do.
///
/// Second-audit Fix 5: the existence probe loop is wrapped in a single
/// `spawn_blocking` call so the entire walk runs off the async event loop.
/// For the common case of 0–2 collisions this is a <100 µs round trip.
async fn find_available_numbered_path(
    initial: std::path::PathBuf,
    exports_dir: &std::path::Path,
    base_slug: &str,
) -> std::path::PathBuf {
    let exports_dir_owned = exports_dir.to_path_buf();
    let exports_dir_fallback = exports_dir_owned.clone();
    let base_slug_owned = base_slug.to_string();
    let base_slug_fallback = base_slug_owned.clone();
    tokio::task::spawn_blocking(move || {
        let mut path = initial;
        let mut n: u64 = 2;
        while path.exists() {
            path = exports_dir_owned.join(format!("{}-{}.md", base_slug_owned, n));
            n = n.saturating_add(1);
        }
        path
    })
    .await
    .unwrap_or_else(|_| exports_dir_fallback.join(format!("{}.md", base_slug_fallback)))
}

/// Story 4-4 AC5: run a cross-conversation search scan and populate
/// `state.cross_search` with the results. Blocks the event loop for up to
/// DEPRECATED in favor of the async-spawned path that delivers results via
/// `AppEvent::CrossSearchResultsReady`. Kept temporarily for reference until
/// Phase 5 E2E tests are rewritten. No longer called from the event loop.
#[allow(dead_code)]
async fn apply_cross_search_scan(
    storage: &dyn StoragePort,
    session_index: &SessionIndex,
    state: &mut TuiState,
) {
    use crate::domain::services::cross_search::{CrossSearchBudget, run_cross_search};
    state.cross_search.running = true;
    let outcome = run_cross_search(
        storage,
        session_index,
        &state.cross_search.query,
        CrossSearchBudget::default(),
    )
    .await;
    state.cross_search.running = false;
    state.cross_search.results = outcome.results;
    state.cross_search.truncated_by_count = outcome.truncated_by_count;
    state.cross_search.truncated_by_time = outcome.truncated_by_time;
    state.cross_search.scanned = outcome.scanned;
    state.cross_search.total = outcome.total;
    // Clamp selection.
    if state.cross_search.results.is_empty() {
        state.cross_search.selected = 0;
    } else {
        state.cross_search.selected = state
            .cross_search
            .selected
            .min(state.cross_search.results.len() - 1);
    }
}

/// Story 4-4 AC6 (amended): open the selected cross-search result in a new
/// tab (or switch to its existing tab), scroll to the target message, and
/// apply a transient peek highlight for 1500 ms.
///
/// Does NOT auto-open the inline search bar — the reviewer's Fix 5 synthesis
/// replaces the original auto-open with a peek highlight, eliminating
/// cognitive whiplash.
#[allow(clippy::too_many_arguments)]
async fn apply_open_cross_search_result(
    tab_manager: &mut TabManager,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    session_manager: &mut SessionManager,
    state: &mut TuiState,
    turn_queue: &mut TurnQueue,
    storage: &dyn StoragePort,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
    session_index: &mut SessionIndex,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    // Resolve the selected result.
    let Some(result) = state
        .cross_search
        .results
        .get(state.cross_search.selected)
        .cloned()
    else {
        return;
    };

    // If already open in a tab, switch to it. Otherwise load and create new.
    let existing_tab_idx = tab_manager
        .tabs()
        .iter()
        .position(|t| t.conversation.id == result.conversation_id);

    if let Some(idx) = existing_tab_idx {
        save_active_tab(
            tab_manager,
            conversation,
            streaming,
            session_manager,
            state,
            turn_queue,
        );
        tab_manager.switch_to_index(idx + 1); // 1-based
        let _ = load_active_tab(
            tab_manager,
            conversation,
            streaming,
            session_manager,
            state,
            turn_queue,
        );
    } else {
        match storage.load_conversation(&result.conversation_id).await {
            Ok(Some(loaded)) => {
                save_active_tab(
                    tab_manager,
                    conversation,
                    streaming,
                    session_manager,
                    state,
                    turn_queue,
                );
                tab_manager.create_tab_with_conversation(loaded.clone());
                // Hydrate session_meta (bookmarks) for the new tab.
                if let Ok(Some(meta)) = fs_storage.load_session_meta(&result.conversation_id).await
                {
                    tab_manager.active_tab_mut().session_meta = meta;
                }
                *conversation = loaded;
                session_index.set_open(&result.conversation_id, true);
                session_index.set_active(Some(&result.conversation_id));
            }
            Ok(None) => {
                tracing::warn!(
                    "cross-search: conversation {} not found at open time",
                    result.conversation_id
                );
                return;
            }
            Err(e) => {
                tracing::error!(
                    "cross-search: load_conversation({}) failed: {}",
                    result.conversation_id,
                    e
                );
                state.status = StatusState::Flash {
                    message: format!("Failed to open conversation: {}", e),
                    remaining_ms: 3000,
                };
                state.needs_redraw = true;
                return;
            }
        }
    }

    // Stash the cross-search query before resetting the overlay, so we can
    // pre-populate the inline search bar on the next Ctrl+F press (AC6
    // amendment: we do NOT auto-open the bar, but we DO preserve the query
    // in case the user wants to jump to other matches in the same
    // conversation).
    let preserved_query = state.cross_search.query.clone();

    // Close the cross-search overlay.
    state.cross_search = crate::adapters::tui::state::CrossSearchState::new();

    // Scroll to the target message. `message_boundaries` for the newly
    // activated tab will be computed on the next render tick, so this scroll
    // may be briefly off until the first repaint — acceptable for v1.
    use crate::adapters::tui::widgets::chat_pane;
    state.scroll_offset = chat_pane::find_scroll_offset_for_message(
        result.first_match_message_index,
        &state.message_boundaries,
        state.total_content_height,
        state.terminal_height as usize,
    );
    state.auto_scroll = state.scroll_offset == 0;
    state.focus = FocusState::Chat;

    // Preserve the query on state.search_state.query (inactive) so Ctrl+F
    // opens the bar pre-populated. AC6 amendment: no auto-open.
    state.search_state.query = preserved_query.clone();

    // Set a 1500 ms peek highlight on the cross-search flagged match in the
    // newly loaded conversation (AC6 — replaces the v1 status flash).
    //
    // Second-audit Fix 6: target the match in `result.first_match_message_index`
    // explicitly rather than relying on `matches.first()`. Under normal
    // operation the two are the same (because `find_matches` is deterministic
    // and returns matches sorted by `(message_index, byte_start)`), but the
    // explicit filter is defensive against conversation mutation between scan
    // and open, and makes the intent visible to future readers.
    //
    // The event loop wakes itself via AppEvent::PeekHighlightExpired to clear
    // the highlight on time even without further user input.
    if !preserved_query.is_empty() {
        use crate::adapters::tui::state::PeekHighlight;
        use crate::domain::services::search::find_matches;
        let matches = find_matches(conversation, &preserved_query);
        let target = matches
            .iter()
            .find(|m| m.message_index == result.first_match_message_index)
            .or_else(|| matches.first());
        if let Some(target_match) = target {
            let expires_at = std::time::Instant::now() + std::time::Duration::from_millis(1500);
            state.search_state.peek_highlight = Some(PeekHighlight {
                m: target_match.clone(),
                expires_at,
            });
            // Schedule the expiry event: tokio::time::sleep_until the deadline,
            // then send PeekHighlightExpired to the event loop so the render
            // loop clears the highlight on time even if no other input occurs.
            let tx = domain_tx.clone();
            let tab_id = tab_manager.active_tab_index();
            tokio::spawn(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(expires_at)).await;
                let _ = tx.send(AppEvent::PeekHighlightExpired { tab_id });
            });
        }
    }
    state.needs_redraw = true;
}

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
    tab.user_message_boundaries = state.user_message_boundaries.clone();
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
    state.user_message_boundaries = tab.user_message_boundaries.clone();
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
/// Decode, hash, and persist the `pending_images` drained on message submit.
///
/// Returns the image refs to attach to the new user `ChatMessage` so they survive
/// a reload (Story 4-3a.1 AC3). Failures are logged via `tracing::warn!` and the
/// corresponding attachment is dropped from the persisted list — AC4's
/// graceful-degradation on load handles missing files if a partial save occurs.
fn persist_image_attachments(
    conversation_id: &str,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
    attachments: &[ImageAttachment],
) -> Vec<crate::domain::models::ImageReference> {
    use crate::adapters::filesystem::{content_hash, normalize_extension};
    use base64::{Engine, engine::general_purpose::STANDARD};

    // Guard: base64 encoding inflates size by ~4/3. MAX_RAW_IMAGE_SIZE is 20MB, so the
    // encoded ceiling is ceil(20MB * 4/3) ≈ 27MB. Reject larger input before decoding to
    // prevent memory exhaustion from a malformed or hostile attachment (P1, party-mode
    // review 2026-04-12). The upstream paste handler already enforces MAX_RAW_IMAGE_SIZE
    // on the raw bytes; this is a second line of defence at the persistence boundary.
    const MAX_BASE64_ENCODED_SIZE: usize = 27 * 1024 * 1024; // 27MB

    let mut refs = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        if attachment.data.len() > MAX_BASE64_ENCODED_SIZE {
            tracing::warn!(
                size = attachment.data.len(),
                limit = MAX_BASE64_ENCODED_SIZE,
                "Skipping image attachment: base64 data exceeds size limit"
            );
            continue;
        }
        let bytes = match STANDARD.decode(attachment.data.as_bytes()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("Failed to base64-decode pending image: {}", e);
                continue;
            }
        };
        let hash = content_hash(&bytes);
        let ext = normalize_extension(&attachment.media_type);
        let file_name = format!("{}.{}", hash, ext);
        let image_ref = crate::domain::models::ImageReference {
            file_name,
            media_type: attachment.media_type.clone(),
            original_size: bytes.len(),
        };
        // Block on the async save via tokio's current-runtime handle. This
        // runs inside the event loop task which already holds the runtime
        // context, so `block_in_place` + a short-lived local is sufficient.
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(fs_storage.save_image(
                conversation_id,
                &image_ref,
                &bytes,
            ))
        });
        match result {
            Ok(()) => refs.push(image_ref),
            Err(e) => {
                tracing::warn!(
                    conversation_id,
                    file_name = %image_ref.file_name,
                    "Failed to persist image attachment: {}",
                    e
                );
                // Still record the ref — the ChatMessage will carry it, and
                // load-time validation will flag the missing file to the user.
                refs.push(image_ref);
            }
        }
    }
    refs
}

/// Walk `conversation.messages` in parallel with the `Vec<Message>` produced
/// by `build_api_messages` and, for each historical user message that carries
/// persisted `ImageReference` entries, load the bytes from disk, base64-encode
/// them, and attach the resulting `ImageAttachment` blocks to the matching
/// API `Message`.
///
/// This is the runtime half of DF-067: Story 4-3a.1's original scope only
/// covered persisting images to disk so they survive reload. It did not cover
/// re-attaching them to the outbound API request on turn N+1, which meant the
/// assistant "forgot" images across turns. Addendum 2 (2026-04-12 party-mode
/// follow-up) closes that gap.
///
/// Messages whose `images` vec is already populated are skipped — the fresh
/// turn's just-submitted message already has raw base64 attached by the
/// caller and does not need a disk read.
///
/// Missing files are logged via `tracing::warn!` and the corresponding block
/// is dropped from the request (AC4 graceful degradation extended to the API
/// request path).
///
/// The index-advance logic must stay in sync with `build_api_messages`: it
/// emits one `Message` per `ChatMessage`, plus a second synthetic user
/// `Message` when an assistant turn carries tool-call results.
///
/// NOTE on `block_in_place`: mirrors `persist_image_attachments` — both call
/// sites share the same sync-over-async pattern and will be lifted together
/// whenever DF-102 (async conversion of `start_turn`) is resolved.
pub fn rehydrate_historical_images(
    conversation: &Conversation,
    messages: &mut [crate::domain::models::Message],
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
) {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let mut msg_idx = 0;
    for cm in &conversation.messages {
        if msg_idx >= messages.len() {
            break;
        }

        if cm.role == MessageRole::User
            && !cm.images.is_empty()
            && messages[msg_idx].images.is_empty()
        {
            for image_ref in &cm.images {
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(fs_storage.load_image(&conversation.id, image_ref))
                });
                match result {
                    Ok(bytes) => {
                        let data = STANDARD.encode(&bytes);
                        messages[msg_idx].images.push(ImageAttachment {
                            media_type: image_ref.media_type.clone(),
                            data,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            conversation_id = %conversation.id,
                            file_name = %image_ref.file_name,
                            "Skipping historical image in API request: {}",
                            e
                        );
                    }
                }
            }
        }

        msg_idx += 1;

        // Mirror `build_api_messages`: assistant messages with at least one
        // tool-call result produce a second synthetic user message for the
        // tool results.
        if cm.role == MessageRole::Assistant
            && !cm.tool_calls.is_empty()
            && cm.tool_calls.iter().any(|tc| tc.result.is_some())
        {
            msg_idx += 1;
        }
    }
}

/// Find the checkpoint ID that covers the given message index.
///
/// Returns the checkpoint with the highest ID where `meta.message_index <= target_message_index`.
/// Falls back to `CheckpointId(0)` when no checkpoint is found (no tool calls were made, or
/// the conversation has no checkpoint log). A sentinel of 0 means "revert nothing" — callers
/// should still truncate messages, but file-snapshot reversal will be a no-op since no snapshots
/// exist with cp_id > 0.
async fn resolve_checkpoint_for_message(
    storage: &Arc<dyn StoragePort>,
    conversation_id: &str,
    target_message_index: usize,
) -> crate::domain::models::checkpoint::CheckpointId {
    use crate::domain::models::checkpoint::CheckpointId;
    let checkpoints = storage
        .list_checkpoints(conversation_id)
        .await
        .unwrap_or_default();
    checkpoints
        .iter()
        .filter(|c| c.message_index <= target_message_index)
        .max_by_key(|c| c.id)
        .map(|c| c.id)
        .unwrap_or(CheckpointId(0))
}

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
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
    storage: &Arc<dyn StoragePort>,
) {
    // Persist any image attachments and collect their references. These are
    // attached to the user ChatMessage so they survive a session reload
    // (Story 4-3a.1 AC3 / DF-067).
    let persisted_refs = if images.is_empty() {
        Vec::new()
    } else {
        persist_image_attachments(&conversation.id, fs_storage, &images)
    };

    // Add user ChatMessage to conversation
    conversation.messages.push(ChatMessage {
        id: generate_conversation_id(),
        role: MessageRole::User,
        content: text.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: crate::domain::models::session_meta::now_unix(),
        token_count: None,
        stop_reason: None,
        images: persisted_refs,
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

    // Rehydrate historical images from disk so the provider sees the same
    // visual context on every subsequent turn (Story 4-3a.1 Addendum 2 /
    // multi-turn image rehydration). `build_api_messages` is a pure domain
    // function and cannot touch disk, so rehydration happens here — after
    // the fresh-turn attachment above, so the just-submitted message is
    // skipped (its `images` vec is already populated with the raw base64
    // and we don't need to re-read it from disk).
    rehydrate_historical_images(conversation, &mut messages, fs_storage);

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
        storage.clone(),
    ));
    *active_turn = Some(handle);

    state.status = StatusState::Streaming;
    state.needs_redraw = true;
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
    session_index: &SessionIndex,
) -> Result<()> {
    let scroll_offset = state.scroll_offset;
    let auto_scroll = state.auto_scroll;
    let mut content_height = 0usize;
    let mut block_bounds: Vec<usize> = Vec::new();
    let mut msg_bounds: Vec<usize> = Vec::new();
    let mut user_msg_bounds: Vec<usize> = Vec::new();
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
        ref focus,
        has_project_context,
        multiline_mode,
        input_scroll_offset,
        ref reverse_search,
        ref search_state,
        ref autocomplete,
        ref command_palette,
        ref which_key,
        ref help_overlay,
        ref image_indicator,
        ref current_hint,
        sidebar_visible,
        ref pending_fork_index,
        ref pending_rewind_index,
        ref rewind_preview,
        ..
    } = *state;

    terminal.draw(|frame| {
        let area = frame.area();

        match layout::compute_layout(area, theme, input_buffer, tab_count, sidebar_visible) {
            Some(mut app_layout) => {
                let _is_compact = area.width < 80 || area.height < 24;

                // Story 4-4 AC1: reserve the top row for the search bar when active.
                app_layout.reserve_search_bar(search_state.active);
                // Story 4-4 AC10: reserve the bottom panel when the bookmark
                // list overlay is focused. Panel height scales with the
                // bookmark count (1 header + N entries + 1 footer, capped at 6).
                if focus
                    == &FocusState::Overlay(
                        crate::domain::models::visual::OverlayType::BookmarkList,
                    )
                {
                    let bookmark_count = tab_manager_for_bar
                        .map(|tm| tm.active_tab().session_meta.bookmarks.len())
                        .unwrap_or(0);
                    let requested_height = ((bookmark_count + 3).min(6)) as u16;
                    app_layout.reserve_bookmark_panel(requested_height);
                }

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

                // P1/AC1: Render sidebar when visible
                if let Some(sidebar_area) = app_layout.sidebar {
                    let active_conv_id =
                        tab_manager_for_bar.map(|tm| tm.active_tab().conversation.id.as_str());
                    sidebar::render_history_panel(
                        sidebar_area,
                        frame.buffer_mut(),
                        session_index.entries(),
                        state.sidebar_selected,
                        active_conv_id,
                        theme,
                    );
                }

                // Story 4-4 AC2: thread search query + focused match into chat pane
                // rendering so substring matches render with highlight styling.
                let search_query_opt: Option<&str> =
                    if search_state.active && !search_state.query.is_empty() {
                        Some(search_state.query.as_str())
                    } else {
                        None
                    };
                let focused_match_opt = if search_state.active {
                    search_state.matches.get(search_state.focused_match_index)
                } else {
                    None
                };

                // Story 4-4 AC9: source the bookmark list from the active
                // tab's session_meta (in-memory mirror of meta.json).
                let bookmark_indices: &[usize] = tab_manager_for_bar
                    .map(|tm| tm.active_tab().session_meta.bookmarks.as_slice())
                    .unwrap_or(&[]);
                let result = chat_pane::render_with_search(
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
                    search_query_opt,
                    focused_match_opt,
                    search_state.matches.as_slice(),
                    bookmark_indices,
                );

                // Story 4-4 AC1: render the search bar widget into its reserved slot.
                if let Some(search_bar_area) = app_layout.search_bar {
                    crate::adapters::tui::widgets::search_bar::render(
                        frame,
                        search_bar_area,
                        search_state,
                        theme,
                    );
                }

                // Story 4-4 AC5: render cross-search overlay inside the sidebar column.
                if state.cross_search.active {
                    if let Some(sidebar_area) = app_layout.sidebar {
                        crate::adapters::tui::widgets::cross_search::render(
                            frame,
                            sidebar_area,
                            &state.cross_search,
                            theme,
                        );
                    }
                }

                // Story 4-4 AC10: render the bookmark list panel into its reserved slot.
                if let Some(bookmark_panel_area) = app_layout.bookmark_panel {
                    let bookmarks: &[usize] = tab_manager_for_bar
                        .map(|tm| tm.active_tab().session_meta.bookmarks.as_slice())
                        .unwrap_or(&[]);
                    crate::adapters::tui::widgets::bookmark_list::render(
                        frame,
                        bookmark_panel_area,
                        conversation,
                        bookmarks,
                        state.bookmark_list_selected,
                        theme,
                    );
                }
                content_height = result.total_content_height;
                block_bounds = result.block_boundaries;
                msg_bounds = result.message_boundaries;
                user_msg_bounds = result.user_message_boundaries;
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

                // Render fork confirmation card at bottom of chat pane if pending (Story 4-3a, AC1)
                if *focus
                    == FocusState::Overlay(
                        crate::domain::models::visual::OverlayType::Confirmation(
                            crate::domain::models::visual::ConfirmationType::Fork,
                        ),
                    )
                {
                    if let Some(fork_idx) = pending_fork_index {
                        use crate::adapters::tui::widgets::fork_confirm;
                        // Get message preview from conversation
                        let preview = conversation
                            .messages
                            .get(*fork_idx)
                            .map(|m| m.content.as_str())
                            .unwrap_or("");
                        let fork_lines = fork_confirm::render_fork_confirmation_lines(
                            preview,
                            *fork_idx,
                            app_layout.chat_pane.width,
                            theme,
                        );
                        let fork_height = fork_lines.len() as u16;
                        let fork_area = ratatui::prelude::Rect {
                            x: app_layout.chat_pane.x,
                            y: app_layout.chat_pane.y + app_layout.chat_pane.height
                                - fork_height.min(app_layout.chat_pane.height),
                            width: app_layout.chat_pane.width,
                            height: fork_height.min(app_layout.chat_pane.height),
                        };
                        let paragraph = ratatui::widgets::Paragraph::new(fork_lines);
                        frame.render_widget(ratatui::widgets::Clear, fork_area);
                        frame.render_widget(paragraph, fork_area);
                    }
                }

                // Render rewind confirmation card at bottom of chat pane if pending (Story 4-3b, AC1)
                if *focus
                    == FocusState::Overlay(
                        crate::domain::models::visual::OverlayType::Confirmation(
                            crate::domain::models::visual::ConfirmationType::Rewind,
                        ),
                    )
                {
                    let _ = pending_rewind_index; // used by handler; preview drives render
                    if let Some(preview) = rewind_preview {
                        use crate::adapters::tui::widgets::rewind_confirm;
                        let rewind_lines = rewind_confirm::render_rewind_confirmation_lines(
                            preview,
                            app_layout.chat_pane.width,
                            theme,
                        );
                        let rewind_height = rewind_lines.len() as u16;
                        let rewind_area = ratatui::prelude::Rect {
                            x: app_layout.chat_pane.x,
                            y: app_layout.chat_pane.y + app_layout.chat_pane.height
                                - rewind_height.min(app_layout.chat_pane.height),
                            width: app_layout.chat_pane.width,
                            height: rewind_height.min(app_layout.chat_pane.height),
                        };
                        let paragraph = ratatui::widgets::Paragraph::new(rewind_lines);
                        frame.render_widget(ratatui::widgets::Clear, rewind_area);
                        frame.render_widget(paragraph, rewind_area);
                    }
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
                    focus.clone(),
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
    state.user_message_boundaries = user_msg_bounds;
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

    // P1 (party-mode review 2026-04-12): base64 size guard in persist_image_attachments
    #[test]
    fn test_persist_image_attachments_rejects_oversized_base64() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::domain::models::ImageAttachment;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        // Synthesize a base64 string that exceeds MAX_BASE64_ENCODED_SIZE (27MB).
        // 28MB of 'A' characters (all-same base64 input is fine for this test).
        let oversized_base64 = "A".repeat(28 * 1024 * 1024);
        let attachments = vec![ImageAttachment {
            media_type: "image/png".to_string(),
            data: oversized_base64,
        }];
        let refs = persist_image_attachments("test-conv-p1", &storage, &attachments);
        assert!(
            refs.is_empty(),
            "oversized base64 attachment must be dropped, got {} refs",
            refs.len()
        );
    }

    // P1: normal-sized attachment passes the guard and produces a ref.
    // Requires multi_thread flavor because persist_image_attachments uses
    // block_in_place internally (pre-existing DF-097).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_persist_image_attachments_accepts_valid_small_attachment() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::domain::models::ImageAttachment;
        use base64::{Engine, engine::general_purpose::STANDARD};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        // 4 bytes of PNG → valid small attachment
        let data = STANDARD.encode(b"\x89PNG");
        let attachments = vec![ImageAttachment {
            media_type: "image/png".to_string(),
            data,
        }];
        let refs = persist_image_attachments("test-conv-p1-ok", &storage, &attachments);
        assert_eq!(
            refs.len(),
            1,
            "valid small attachment must produce one ImageReference"
        );
        assert!(refs[0].file_name.ends_with(".png"));
    }

    // ── Addendum 2 (2026-04-12): Multi-turn image rehydration ──────────
    // See `rehydrate_historical_images` for the full rationale. These tests
    // cover the unit-level contract: given a Conversation with persisted
    // ImageReferences, the output `Vec<Message>` must carry the bytes as
    // ImageAttachment blocks on the matching User messages.

    fn mk_chat_msg_user_with_images(
        content: &str,
        images: Vec<crate::domain::models::ImageReference>,
    ) -> ChatMessage {
        ChatMessage {
            id: generate_conversation_id(),
            role: MessageRole::User,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000_000,
            token_count: None,
            stop_reason: None,
            images,
        }
    }

    fn mk_chat_msg_assistant(content: &str) -> ChatMessage {
        ChatMessage {
            id: generate_conversation_id(),
            role: MessageRole::Assistant,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000_001,
            token_count: None,
            stop_reason: None,
            images: vec![],
        }
    }

    fn mk_conversation(id: &str, messages: Vec<ChatMessage>) -> Conversation {
        Conversation {
            id: id.to_string(),
            title: "rehydrate test".to_string(),
            messages,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_001,
            last_response_at: None,
            session_id: None,
            usage: None,
            fork_source: None,
        }
    }

    /// Multi-turn happy path: image saved on turn 1 is rehydrated into the
    /// API request on turn 2. This is the core regression test for the
    /// reported bug ("assistant forgets image on next turn").
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rehydrate_historical_images_multi_turn_happy_path() {
        use crate::adapters::filesystem::{FileSystemStorage, content_hash, normalize_extension};
        use crate::domain::models::ImageReference;
        use base64::{Engine, engine::general_purpose::STANDARD};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv_id = "conv-rehydrate-happy";

        // Given: a user turn-1 message with an image persisted on disk.
        let bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n-turn1-payload".to_vec();
        let file_name = format!(
            "{}.{}",
            content_hash(&bytes),
            normalize_extension("image/png")
        );
        let image_ref = ImageReference {
            file_name: file_name.clone(),
            media_type: "image/png".to_string(),
            original_size: bytes.len(),
        };
        storage
            .save_image(conv_id, &image_ref, &bytes)
            .await
            .unwrap();

        // Conversation state on turn 2: turn-1 user msg (with persisted ref),
        // turn-1 assistant reply, turn-2 user msg (fresh text, no images).
        let conv = mk_conversation(
            conv_id,
            vec![
                mk_chat_msg_user_with_images("what is this?", vec![image_ref.clone()]),
                mk_chat_msg_assistant("a cat"),
                mk_chat_msg_user_with_images("what colour?", vec![]),
            ],
        );

        let mut messages = message_builder::build_api_messages(&conv);
        assert_eq!(messages.len(), 3);
        assert!(messages[0].images.is_empty(), "pre-rehydrate: empty");

        // When: rehydration runs
        rehydrate_historical_images(&conv, &mut messages, &storage);

        // Then: turn-1 user message carries the image bytes as base64 in
        // its API image attachment, and turn-2 remains empty (no ref).
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[0].images.len(), 1, "turn-1 must be rehydrated");
        assert_eq!(messages[0].images[0].media_type, "image/png");
        let expected_b64 = STANDARD.encode(&bytes);
        assert_eq!(messages[0].images[0].data, expected_b64);

        assert_eq!(messages[2].role, MessageRole::User);
        assert!(messages[2].images.is_empty(), "turn-2 has no image ref");
    }

    /// A message whose `images` vec is already populated (e.g. fresh turn
    /// attachment happened before rehydrate ran) must NOT be touched. This
    /// guards against double-attaching the same image twice in the same
    /// API request.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rehydrate_skips_already_populated_messages() {
        use crate::adapters::filesystem::{FileSystemStorage, content_hash, normalize_extension};
        use crate::domain::models::ImageReference;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv_id = "conv-rehydrate-skip";

        let bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n-skip".to_vec();
        let file_name = format!(
            "{}.{}",
            content_hash(&bytes),
            normalize_extension("image/png")
        );
        let image_ref = ImageReference {
            file_name,
            media_type: "image/png".to_string(),
            original_size: bytes.len(),
        };
        storage
            .save_image(conv_id, &image_ref, &bytes)
            .await
            .unwrap();

        let conv = mk_conversation(
            conv_id,
            vec![mk_chat_msg_user_with_images("hi", vec![image_ref.clone()])],
        );

        let mut messages = message_builder::build_api_messages(&conv);
        // Pre-populate with a sentinel "fresh" attachment — rehydrate must leave it alone.
        messages[0].images.push(ImageAttachment {
            media_type: "image/png".to_string(),
            data: "SENTINEL-FRESH".to_string(),
        });

        rehydrate_historical_images(&conv, &mut messages, &storage);

        assert_eq!(messages[0].images.len(), 1, "must not double-attach");
        assert_eq!(messages[0].images[0].data, "SENTINEL-FRESH");
    }

    /// Missing image file on disk must not panic or fail the turn — just
    /// warn and skip. This extends AC4 graceful degradation to the API
    /// request path.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rehydrate_missing_file_degrades_gracefully() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::domain::models::ImageReference;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv_id = "conv-rehydrate-missing";

        // Dangling reference: valid shape (16 hex chars + .png) but no file.
        let image_ref = ImageReference {
            file_name: "deadbeefdeadbeef.png".to_string(),
            media_type: "image/png".to_string(),
            original_size: 42,
        };

        let conv = mk_conversation(
            conv_id,
            vec![mk_chat_msg_user_with_images("ghost", vec![image_ref])],
        );

        let mut messages = message_builder::build_api_messages(&conv);
        rehydrate_historical_images(&conv, &mut messages, &storage);

        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].images.is_empty(),
            "missing image must be dropped, not panicked"
        );
    }

    /// Index-parity regression: when an assistant message with tool-call
    /// results sits between two user messages, `build_api_messages` emits
    /// an extra synthetic user message (for tool results). Rehydration
    /// must still land images on the correct turn-1 user message.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rehydrate_index_parity_with_tool_results() {
        use crate::adapters::filesystem::{FileSystemStorage, content_hash, normalize_extension};
        use crate::domain::models::ImageReference;
        use crate::domain::models::{ToolCallInfo, ToolResultInfo};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv_id = "conv-rehydrate-tools";

        let bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n-tools".to_vec();
        let file_name = format!(
            "{}.{}",
            content_hash(&bytes),
            normalize_extension("image/png")
        );
        let image_ref = ImageReference {
            file_name,
            media_type: "image/png".to_string(),
            original_size: bytes.len(),
        };
        storage
            .save_image(conv_id, &image_ref, &bytes)
            .await
            .unwrap();

        // turn-1: user with image
        // turn-1: assistant with a tool call + result (→ 2 API messages)
        // turn-2: user with no image
        let mut assistant_with_tool = mk_chat_msg_assistant("calling tool");
        assistant_with_tool.tool_calls.push(ToolCallInfo {
            id: "tool-1".to_string(),
            name: "Read".to_string(),
            input: serde_json::json!({"path": "x"}),
            result: Some(ToolResultInfo {
                content: "file contents".to_string(),
                is_error: false,
            }),
            started_at_ms: None,
            completed_at_ms: None,
        });
        let conv = mk_conversation(
            conv_id,
            vec![
                mk_chat_msg_user_with_images("analyse this", vec![image_ref.clone()]),
                assistant_with_tool,
                mk_chat_msg_user_with_images("more?", vec![]),
            ],
        );

        let mut messages = message_builder::build_api_messages(&conv);
        // Expected layout: [User(analyse), Assistant(calling), User(tool-result), User(more)]
        assert_eq!(messages.len(), 4, "tool-result synthetic message present");

        rehydrate_historical_images(&conv, &mut messages, &storage);

        // Image lands on messages[0], NOT on the synthetic tool-result at [2].
        assert_eq!(
            messages[0].images.len(),
            1,
            "image must rehydrate onto turn-1 user message"
        );
        assert!(
            messages[2].images.is_empty(),
            "synthetic tool-result user message must not receive images"
        );
        assert!(
            messages[3].images.is_empty(),
            "turn-2 user message has no image ref"
        );
    }
}
