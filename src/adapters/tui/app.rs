use crate::adapters::tui::state::{ChordAction, Direction, TuiState};
use crate::adapters::tui::widgets::chat_pane::virtual_scroll::find_next_boundary;
use crate::adapters::tui::widgets::input_box;
use crate::domain::events::{DomainInputEvent, DomainKey};
use crate::domain::models::FocusState;
use crate::domain::models::autocomplete::AutocompleteKind;
use crate::domain::models::visual::{ConfirmationType, OverlayType};

/// Action returned by handle_input to tell the event loop what to do.
/// app.rs is a pure input→action mapper; the event loop owns all side effects.
#[derive(Debug, PartialEq, Eq)]
pub enum InputAction {
    /// Event handled, no further action needed.
    Consumed,
    /// Event not handled by this focus mode.
    Ignored,
    /// Enter pressed with this text (buffer already cleared by handle_input).
    SubmitMessage(String),
    /// User wants to exit.
    Quit,
    /// Ctrl+C: cancel streaming if active, otherwise quit.
    CancelOrQuit,
    /// Permission prompt: user pressed y.
    PermissionAllow,
    /// Permission prompt: user pressed n.
    PermissionDeny,
    /// Permission prompt: user pressed a.
    PermissionAlwaysAllow,
    /// Feedback block: user pressed r to retry.
    FeedbackRetry,
    /// AskUserQuestion: user submitted their answer.
    SubmitQuestionAnswer(String),
    /// Execute a built-in command (e.g., "/new").
    ExecuteCommand(String),
    /// Submit message with file context and/or command context.
    /// Contains (user_text, resolved_mentions, command_name_if_any).
    SubmitWithContext {
        text: String,
        command: Option<String>,
    },
    /// Create a new tab (Ctrl+T or palette).
    NewTab,
    /// Close the active tab (palette).
    CloseTab,
    /// Switch to the next tab (Tab key when focus is Chat).
    SwitchToNextTab,
    /// Switch to the previous tab (Shift+Tab when focus is not Input).
    SwitchToPrevTab,
    /// Switch directly to a tab by 1-based index (number keys 1-9 in Chat focus).
    SwitchToTab(usize),
    /// Toggle the history sidebar (Ctrl+H).
    // Covers: FR107, UX-DR20
    ToggleSidebar,
    /// Open the selected conversation from sidebar.
    // Covers: FR107, AC4
    OpenSidebarConversation,
    /// Delete the selected conversation from sidebar (shows confirmation overlay).
    // Covers: FR113, AC5
    DeleteSidebarConversation,
    /// Delete all conversations (via command palette).
    // Covers: AC5 (bulk), P9
    DeleteAllConversations,
    /// User confirmed pending delete (pressed 'y').
    ConfirmDelete,
    /// User cancelled pending delete (pressed 'n' or Esc).
    CancelDelete,
    /// Copy content to clipboard.
    // Covers: FR116, UX-DR68
    CopyToClipboard(String),
    /// User confirmed attaching a large image.
    // Covers: FR112 (AC4)
    ImageConfirmAttach,
    /// User cancelled attaching a large image.
    // Covers: FR112 (AC4)
    ImageConfirmCancel,
    /// Pasted image format is unsupported — show FeedbackBlock.
    // Covers: FR112 (AC3)
    ImageFormatError,
    /// Pasted image exceeds size threshold — needs user confirmation.
    // Covers: FR112 (AC4)
    ImageSizeWarning {
        media_type: String,
        data: String,
        warning: String,
    },
    /// Request a system-clipboard paste: read image (or text) from the OS clipboard.
    /// The event loop handles this asynchronously via ClipboardPort, then re-enters
    /// handle_input with the resulting ImagePaste or Paste event.
    RequestClipboardPaste,
    /// Fork conversation at the currently focused message (f key in Chat focus).
    // Covers: Story 4-3a, AC1
    ForkAtMessage,
    /// Fork confirmation: user pressed y.
    // Covers: Story 4-3a, AC1
    ForkConfirm,
    /// Fork confirmation: user pressed n or Esc.
    // Covers: Story 4-3a, AC1
    ForkCancel,
}

/// Handle a domain input event by updating TUI state.
/// Returns an InputAction telling the event loop what to do.
pub fn handle_input(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    match event {
        DomainInputEvent::KeyPress(c) => handle_char(state, *c),
        DomainInputEvent::SpecialKey(key) => handle_special_key(state, *key),
        DomainInputEvent::Resize(w, h) => {
            // Anchor-based scroll preservation: find the message index at the
            // top of the viewport using the *old* HeightCache before invalidation.
            if state.scroll_offset > 0 && !state.block_boundaries.is_empty() {
                let old_vp = state.terminal_height as usize;
                let max_offset = state.total_content_height.saturating_sub(old_vp);
                let clamped = state.scroll_offset.min(max_offset);
                let top_line = max_offset.saturating_sub(clamped);

                // Find which message index contains this top_line by scanning
                // block_boundaries (sorted). The last boundary <= top_line is the anchor.
                let anchor_idx = match state.block_boundaries.binary_search(&top_line) {
                    Ok(i) => i,
                    Err(i) => i.saturating_sub(1),
                };
                state.pending_anchor = Some(anchor_idx);
            }

            state.terminal_width = *w;
            state.terminal_height = *h;
            state.height_cache.invalidate_all();
            // P11: Reset sidebar if terminal shrinks below minimum width
            if *w < crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH && state.sidebar_visible {
                state.sidebar_visible = false;
                state.sidebar_panel = None;
                if matches!(state.focus, FocusState::Sidebar { .. }) {
                    state.focus = FocusState::Chat;
                }
            }
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainInputEvent::FocusGained | DomainInputEvent::FocusLost => {
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainInputEvent::ImagePaste(raw_bytes) => {
            // Validate and attach image from clipboard paste
            // Covers: FR112 (AC1)
            use crate::adapters::tui::image;

            // P1: Reject oversized images before base64 encoding to prevent OOM
            const MAX_RAW_IMAGE_SIZE: usize = 20 * 1024 * 1024; // 20MB
            if raw_bytes.len() > MAX_RAW_IMAGE_SIZE {
                return InputAction::ImageFormatError; // Reuse error path — too large to process
            }

            match image::detect_image_format(raw_bytes) {
                Ok(media_type) => {
                    use base64::Engine;
                    let data = base64::engine::general_purpose::STANDARD.encode(raw_bytes);
                    let base64_len = data.len();

                    // Check size threshold
                    if let Some(warning) = image::validate_image_size(base64_len) {
                        return InputAction::ImageSizeWarning {
                            media_type: media_type.to_string(),
                            data,
                            warning,
                        };
                    }

                    let _total_kb = base64_len / 1024;
                    let attachment = crate::domain::models::ImageAttachment {
                        media_type: media_type.to_string(),
                        data,
                    };
                    state.pending_images.push(attachment);
                    state.image_indicator = Some(image::format_image_indicator(
                        state.pending_images.len(),
                        state
                            .pending_images
                            .iter()
                            .fold(0usize, |acc, i| acc.saturating_add(i.data.len() / 1024)),
                    ));
                    state.needs_redraw = true;
                    InputAction::Consumed
                }
                Err(_) => {
                    // Unsupported format — return action for event loop to create FeedbackBlock
                    InputAction::ImageFormatError
                }
            }
        }
        DomainInputEvent::Paste(text) => {
            // Insert text at cursor position (bracketed paste mode)
            if matches!(state.focus, FocusState::Input) {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert_str(byte_pos, text);
                state.cursor_position += text.chars().count();
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
    }
}

/// Convert a char-index to a byte-index in the string.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn handle_char(state: &mut TuiState, c: char) -> InputAction {
    // Command palette: typing updates filter text
    // Covers: UX-DR18
    if state.command_palette.active {
        // Detect scope prefix on first char
        if state.command_palette.filter_text.is_empty() {
            if let Some(scope) =
                crate::adapters::palette_registry::PaletteRegistry::scope_for_prefix(c)
            {
                state.command_palette.current_scope = Some(scope);
                state.command_palette.filter_text.push(c);
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
        }
        state.command_palette.filter_text.push(c);
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    // Help overlay: handle char keys (j/k/g/G scrolling and ? dismiss)
    // Covers: FR108, UX-DR94
    if state.focus == FocusState::Overlay(OverlayType::Help) {
        return handle_help_overlay_char(state, c);
    }

    // Which-key: single char lookup in chord map
    // Covers: UX-DR19
    if state.which_key.active {
        let chord = state.which_key.lookup_chord(c).cloned();
        if let Some(action) = chord {
            let prior_focus = state.which_key.dismiss().unwrap_or(FocusState::Input);
            state.focus = prior_focus.clone();
            match action {
                ChordAction::Noop(msg) => {
                    // Show "Not yet available" feedback
                    let block = crate::domain::models::FeedbackBlock {
                        id: format!("chord-{}", c),
                        level: crate::domain::models::FeedbackLevel::Info,
                        message: msg,
                        actions: Vec::new(),
                    };
                    state.feedback_blocks.insert(block.id.clone(), block);
                }
                ChordAction::ShowHelp => {
                    // Open help overlay
                    state.help_overlay.open(prior_focus);
                    state.focus = FocusState::Overlay(OverlayType::Help);
                }
                ChordAction::OpenPanel(_) => {
                    // Stub for future epics
                }
            }
            state.needs_redraw = true;
            return InputAction::Consumed;
        } else {
            // Invalid key: dismiss silently (AC6)
            state.focus = state.which_key.dismiss().unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            return InputAction::Consumed;
        }
    }

    // Reverse search: typing adds to query
    // Covers: UX-DR74
    if state.reverse_search.active {
        state.reverse_search.query.push(c);
        update_reverse_search_matches(state);
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    // Permission prompt focus: only y/n/a are handled, all others ignored
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission)) {
        return match c {
            'y' => InputAction::PermissionAllow,
            'n' => InputAction::PermissionDeny,
            'a' => InputAction::PermissionAlwaysAllow,
            _ => InputAction::Consumed, // Ignore all other keys
        };
    }

    // AskUserQuestion focus: type into question input buffer
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Question)) {
        if let Some(ref mut aq) = state.ask_user_question {
            aq.input_buffer.push(c);
            aq.cursor_position += 1;
            state.needs_redraw = true;
        }
        return InputAction::Consumed;
    }

    // Any keypress dismisses an active peek overlay (AC5)
    if state.focus == FocusState::Chat {
        let had_peek = state.tool_block_states.values().any(|tbs| tbs.peek_active);
        if had_peek {
            for tbs in state.tool_block_states.values_mut() {
                tbs.peek_active = false;
            }
            state.needs_redraw = true;
            return InputAction::Consumed;
        }
    }

    match state.focus {
        FocusState::Input => {
            // When autocomplete is active, intercept characters to update filter
            if state.autocomplete.active {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert(byte_pos, c);
                state.cursor_position += 1;
                // Extract filter text: everything after the trigger character
                let trigger = state.autocomplete.trigger_position;
                let filter: String = state
                    .input_buffer
                    .chars()
                    .skip(trigger + 1)
                    .take(state.cursor_position.saturating_sub(trigger + 1))
                    .collect();
                // Signal that autocomplete filter needs updating
                // The actual filtering is done by the event loop which has access to registries
                state.autocomplete.filter_text = filter;
                state.needs_redraw = true;
                return InputAction::Consumed;
            }

            // Detect '/' at position 0 → trigger slash command autocomplete
            if c == '/' && state.cursor_position == 0 && state.input_buffer.is_empty() {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert(byte_pos, c);
                state.cursor_position += 1;
                state.autocomplete.active = true;
                state.autocomplete.kind = AutocompleteKind::SlashCommand;
                state.autocomplete.trigger_position = 0;
                state.autocomplete.filter_text.clear();
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
                // Suggestions will be populated by the event loop (lazy loading)
                state.needs_redraw = true;
                return InputAction::Consumed;
            }

            // Detect '@' anywhere → trigger file mention autocomplete
            if c == '@' {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.insert(byte_pos, c);
                let trigger_pos = state.cursor_position;
                state.cursor_position += 1;
                state.autocomplete.active = true;
                state.autocomplete.kind = AutocompleteKind::FileMention;
                state.autocomplete.trigger_position = trigger_pos;
                state.autocomplete.filter_text.clear();
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
                // Suggestions will be populated by the event loop
                state.needs_redraw = true;
                return InputAction::Consumed;
            }

            let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
            state.input_buffer.insert(byte_pos, c);
            state.cursor_position += 1;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        FocusState::Chat => match c {
            // Feedback block action: retry
            'r' if state.active_feedback_id.is_some() => {
                return InputAction::FeedbackRetry;
            }
            // AC4: Large image confirmation
            'y' if state.pending_large_image.is_some() => {
                return InputAction::ImageConfirmAttach;
            }
            'n' if state.pending_large_image.is_some() => {
                return InputAction::ImageConfirmCancel;
            }
            'i' => {
                state.focus = FocusState::Input;
                state.needs_redraw = true;
                InputAction::Consumed
            }
            'q' => InputAction::Quit,
            // j = scroll down (toward newer content) = decrement offset-from-bottom
            // offset=0 means "at bottom"; j moves toward bottom, so offset decreases
            'j' => {
                if state.scroll_offset > 0 {
                    state.scroll_offset -= 1;
                    state.auto_scroll = state.scroll_offset == 0;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
            // k = scroll up (toward older content) = increment offset-from-bottom
            // Clamped to max scrollable range to prevent unbounded growth
            'k' => {
                let max_offset = state
                    .total_content_height
                    .saturating_sub(state.terminal_height as usize);
                if state.scroll_offset < max_offset {
                    state.scroll_offset += 1;
                    state.auto_scroll = false;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
            // G = jump to bottom, re-enable auto-scroll
            'G' => {
                state.scroll_offset = 0;
                state.auto_scroll = true;
                state.needs_redraw = true;
                InputAction::Consumed
            }
            // J (shift+j) = jump to next content block boundary (down, toward newer)
            'J' => {
                if let Some(new_offset) = find_next_boundary(
                    state.scroll_offset,
                    &state.block_boundaries,
                    Direction::Down,
                    state.total_content_height,
                    state.terminal_height as usize,
                ) {
                    state.scroll_offset = new_offset;
                    state.auto_scroll = new_offset == 0;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
            // K (shift+k) = jump to previous content block boundary (up, toward older)
            'K' => {
                if let Some(new_offset) = find_next_boundary(
                    state.scroll_offset,
                    &state.block_boundaries,
                    Direction::Up,
                    state.total_content_height,
                    state.terminal_height as usize,
                ) {
                    state.scroll_offset = new_offset;
                    state.auto_scroll = false;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
            // { = jump to previous user message
            '{' => {
                if let Some(new_offset) = find_next_boundary(
                    state.scroll_offset,
                    &state.message_boundaries,
                    Direction::Up,
                    state.total_content_height,
                    state.terminal_height as usize,
                ) {
                    state.scroll_offset = new_offset;
                    state.auto_scroll = false;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
            // } = jump to next user message
            '}' => {
                if let Some(new_offset) = find_next_boundary(
                    state.scroll_offset,
                    &state.message_boundaries,
                    Direction::Down,
                    state.total_content_height,
                    state.terminal_height as usize,
                ) {
                    state.scroll_offset = new_offset;
                    state.auto_scroll = new_offset == 0;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
            // ? = toggle help overlay
            // Covers: FR108, UX-DR94 (AC1: ? opens help from any non-Input focus)
            '?' => {
                let prior = state.focus.clone();
                state.help_overlay.open(prior);
                state.focus = FocusState::Overlay(OverlayType::Help);
                state.needs_redraw = true;
                return InputAction::Consumed;
            }
            // c = copy focused content to clipboard
            // Covers: FR116, UX-DR68 (AC6, AC7, AC8, AC9)
            'c' => {
                return InputAction::CopyToClipboard(String::new());
            }
            // p = peek preview on focused collapsed tool block
            'p' => {
                if let Some(ref tool_id) = state.focused_tool_id {
                    let entry = state.tool_block_states.entry(tool_id.clone()).or_default();
                    if entry.collapsed {
                        entry.peek_active = !entry.peek_active;
                        state.needs_redraw = true;
                    }
                }
                InputAction::Consumed
            }
            // 1-9 = direct tab switch (AC2: number key direct switch)
            '1'..='9' => {
                let n = (c as u8 - b'0') as usize;
                InputAction::SwitchToTab(n)
            }
            // f = fork conversation at the currently focused message (Story 4-3a, AC1)
            'f' => InputAction::ForkAtMessage,
            _ => InputAction::Ignored,
        },
        FocusState::Sidebar {
            panel: _panel,
            selected: _selected,
        } => {
            match c {
                'j' => {
                    // Move selection down (clamped to entry count)
                    if state.sidebar_entry_count > 0 {
                        let max = state.sidebar_entry_count - 1;
                        if state.sidebar_selected < max {
                            state.sidebar_selected += 1;
                            state.focus = FocusState::Sidebar {
                                panel: crate::domain::models::visual::PanelType::History,
                                selected: state.sidebar_selected,
                            };
                            state.needs_redraw = true;
                        }
                    }
                    InputAction::Consumed
                }
                'k' => {
                    // Move selection up
                    if state.sidebar_selected > 0 {
                        state.sidebar_selected -= 1;
                        state.focus = FocusState::Sidebar {
                            panel: crate::domain::models::visual::PanelType::History,
                            selected: state.sidebar_selected,
                        };
                        state.needs_redraw = true;
                    }
                    InputAction::Consumed
                }
                'd' => {
                    // Delete selected conversation — shows confirmation overlay
                    InputAction::DeleteSidebarConversation
                }
                'q' => InputAction::Quit,
                _ => InputAction::Ignored,
            }
        }
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::DeleteConfirmation(_))) => {
            match c {
                'y' | 'Y' => InputAction::ConfirmDelete,
                'n' | 'N' => InputAction::CancelDelete,
                _ => InputAction::Consumed,
            }
        }
        // Fork confirmation: y = confirm, n = cancel (Story 4-3a, AC1)
        FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork)) => match c {
            'y' => InputAction::ForkConfirm,
            'n' => InputAction::ForkCancel,
            _ => InputAction::Consumed,
        },
        FocusState::Overlay(_) => InputAction::Ignored,
    }
}

fn handle_special_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    // Delete confirmation: Esc → Cancel
    if matches!(
        state.focus,
        FocusState::Overlay(OverlayType::Confirmation(
            ConfirmationType::DeleteConfirmation(_)
        ))
    ) {
        return match key {
            DomainKey::Esc => InputAction::CancelDelete,
            _ => InputAction::Consumed,
        };
    }

    // Fork confirmation: Esc → Cancel (Story 4-3a, AC1)
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Fork)) {
        return match key {
            DomainKey::Esc => InputAction::ForkCancel,
            _ => InputAction::Consumed,
        };
    }

    // Permission prompt: Esc → Deny
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission)) {
        return match key {
            DomainKey::Esc => InputAction::PermissionDeny,
            _ => InputAction::Consumed, // Ignore all other special keys
        };
    }

    // AskUserQuestion: Enter submits, Backspace deletes, Esc cancels
    if state.focus == FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Question)) {
        return match key {
            DomainKey::Enter => {
                if let Some(ref mut aq) = state.ask_user_question {
                    if aq.input_buffer.is_empty() {
                        return InputAction::Consumed; // Don't submit empty answer
                    }
                    let answer = std::mem::take(&mut aq.input_buffer);
                    aq.submitted_answer = Some(answer.clone());
                    aq.cursor_position = 0;
                    state.needs_redraw = true;
                    InputAction::SubmitQuestionAnswer(answer)
                } else {
                    InputAction::Consumed
                }
            }
            DomainKey::Backspace => {
                if let Some(ref mut aq) = state.ask_user_question {
                    if aq.cursor_position > 0 {
                        aq.cursor_position -= 1;
                        aq.input_buffer.pop();
                        state.needs_redraw = true;
                    }
                }
                InputAction::Consumed
            }
            DomainKey::Esc => {
                // Cancel question — dismiss and drop oneshot sender so run_turn gets RecvError
                state.ask_user_question = None;
                drop(state.question_response_tx.take());
                state.focus = FocusState::Input;
                state.needs_redraw = true;
                InputAction::Consumed
            }
            _ => InputAction::Consumed,
        };
    }

    // Any keypress dismisses an active peek overlay (AC5)
    if state.focus == FocusState::Chat {
        let had_peek = state.tool_block_states.values().any(|tbs| tbs.peek_active);
        if had_peek {
            for tbs in state.tool_block_states.values_mut() {
                tbs.peek_active = false;
            }
            state.needs_redraw = true;
            return InputAction::Consumed;
        }
    }

    // Help overlay: route all special keys when active
    // Covers: FR108, UX-DR94
    if state.focus == FocusState::Overlay(OverlayType::Help) {
        return handle_help_overlay_key(state, key);
    }

    // Command palette overlay handling — intercept keys when palette is active
    // Covers: UX-DR18
    if state.command_palette.active {
        return handle_command_palette_key(state, key);
    }

    // Which-key overlay handling — any special key dismisses
    // Covers: UX-DR19
    if state.which_key.active {
        // Special keys (non-char) dismiss which-key without action (AC6)
        state.focus = state.which_key.dismiss().unwrap_or(FocusState::Input);
        state.needs_redraw = true;
        return InputAction::Consumed;
    }

    // Autocomplete overlay handling — intercept keys when autocomplete is active
    // Covers: UX-DR75 (autocomplete)
    if state.autocomplete.active && state.focus == FocusState::Input {
        let result = handle_autocomplete_key(state, key);
        if result != InputAction::Ignored {
            return result;
        }
        // Ignored = autocomplete dismissed, fall through to normal key handling
    }

    // Reverse search overlay handling
    // Covers: UX-DR74 (reverse search)
    // P16: Allow Ctrl+P and Ctrl+X to dismiss reverse search and open their overlays (Tier-2).
    if state.reverse_search.active && !matches!(key, DomainKey::CtrlP | DomainKey::CtrlX) {
        return handle_reverse_search_key(state, key);
    }

    match key {
        DomainKey::Esc => {
            // In multiline mode with content: submit message (alternative send)
            // If navigating history, cancel navigation instead of submitting.
            // Covers: UX-DR76
            if state.focus == FocusState::Input
                && state.multiline_mode
                && !state.input_buffer.is_empty()
            {
                if state.input_history.is_navigating() {
                    state.input_history.reset_navigation();
                    state.input_buffer.clear();
                    state.cursor_position = 0;
                    state.input_scroll_offset = 0;
                    state.needs_redraw = true;
                    return InputAction::Consumed;
                }
                return submit_message(state);
            }
            state.focus = match state.focus {
                FocusState::Input => FocusState::Chat,
                FocusState::Chat => FocusState::Input,
                // AC11: Esc from Sidebar → Chat
                FocusState::Sidebar { .. } => FocusState::Chat,
                FocusState::Overlay(_) => FocusState::Input,
            };
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Shift+Enter: always insert newline (when terminal supports it)
        // Covers: UX-DR76 (Shift+Enter)
        DomainKey::ShiftEnter if state.focus == FocusState::Input => {
            insert_newline(state);
            InputAction::Consumed
        }

        // Alt+Enter: insert newline (VS Code terminal alternative to Shift+Enter)
        // Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment, AC#1
        DomainKey::AltEnter if state.focus == FocusState::Input => {
            insert_newline(state);
            InputAction::Consumed
        }

        // Alt+M: toggle multi-line mode (VS Code terminal alternative to Ctrl+E)
        // Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment, AC#2
        DomainKey::AltM if state.focus == FocusState::Input => {
            state.multiline_mode = !state.multiline_mode;
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Alt+V: paste image (or text) from the system clipboard.
        // The event loop receives RequestClipboardPaste and handles it async
        // via ClipboardPort, then re-enters handle_input with ImagePaste/Paste.
        DomainKey::AltV => InputAction::RequestClipboardPaste,

        // Ctrl+Enter: submit in multiline mode
        // Covers: UX-DR76
        DomainKey::CtrlEnter if state.focus == FocusState::Input => {
            if !state.input_buffer.is_empty() {
                submit_message(state)
            } else {
                InputAction::Consumed
            }
        }

        // Ctrl+P: open command palette (Tier-1 overlays blocked, Tier-2 overlays allowed)
        // P16: ReverseSearch is Tier-2 — Ctrl+P dismisses it and opens palette.
        // Covers: UX-DR18
        DomainKey::CtrlP
            if !matches!(
                state.focus,
                FocusState::Overlay(
                    OverlayType::CommandPalette
                        | OverlayType::WhichKey
                        | OverlayType::ModelSelector
                        | OverlayType::Help
                        | OverlayType::ProfileSwitcher
                        | OverlayType::Confirmation(_)
                )
            ) =>
        {
            // Dismiss autocomplete if active — only one overlay at a time
            if state.autocomplete.active {
                state.autocomplete.dismiss();
            }
            // Dismiss reverse search (Tier-2) before opening palette
            if state.reverse_search.active {
                state.reverse_search.active = false;
            }
            // Determine prior focus for restoration: if coming from ReverseSearch overlay, restore Input
            let prior_focus = match &state.focus {
                FocusState::Overlay(OverlayType::ReverseSearch) => FocusState::Input,
                FocusState::Overlay(OverlayType::Autocomplete(_)) => FocusState::Input,
                other => other.clone(),
            };
            state.command_palette.open(prior_focus);
            state.focus = FocusState::Overlay(OverlayType::CommandPalette);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Ctrl+X: open which-key (Tier-1 overlays blocked, Tier-2 overlays allowed)
        // P16: ReverseSearch is Tier-2 — Ctrl+X dismisses it and opens which-key.
        // Covers: UX-DR19, UX-DR60
        DomainKey::CtrlX
            if !matches!(
                state.focus,
                FocusState::Overlay(
                    OverlayType::CommandPalette
                        | OverlayType::WhichKey
                        | OverlayType::ModelSelector
                        | OverlayType::Help
                        | OverlayType::ProfileSwitcher
                        | OverlayType::Confirmation(_)
                )
            ) =>
        {
            // Dismiss autocomplete if active — only one overlay at a time
            if state.autocomplete.active {
                state.autocomplete.dismiss();
            }
            // Dismiss reverse search (Tier-2) before opening which-key
            if state.reverse_search.active {
                state.reverse_search.active = false;
            }
            // Determine prior focus for restoration
            let prior_focus = match &state.focus {
                FocusState::Overlay(OverlayType::ReverseSearch) => FocusState::Input,
                FocusState::Overlay(OverlayType::Autocomplete(_)) => FocusState::Input,
                other => other.clone(),
            };
            state.which_key.open(prior_focus);
            state.focus = FocusState::Overlay(OverlayType::WhichKey);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Ctrl+E: toggle multi-line mode
        // Covers: UX-DR76 (Ctrl+E fallback)
        DomainKey::CtrlE if state.focus == FocusState::Input => {
            state.multiline_mode = !state.multiline_mode;
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Ctrl+R: activate reverse search
        // Covers: UX-DR74
        DomainKey::CtrlR if state.focus == FocusState::Input => {
            // P16: Dismiss autocomplete if active — only one overlay at a time
            if state.autocomplete.active {
                state.autocomplete.dismiss();
            }
            state.reverse_search.active = true;
            state.reverse_search.query.clear();
            state.reverse_search.matches.clear();
            state.reverse_search.selected_match = 0;
            state.focus = FocusState::Overlay(OverlayType::ReverseSearch);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        DomainKey::Backspace if state.focus == FocusState::Input => {
            if state.cursor_position > 0 {
                state.cursor_position -= 1;
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.remove(byte_pos);
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        // Delete key: remove character at cursor position
        DomainKey::Delete if state.focus == FocusState::Input => {
            let total_chars = state.input_buffer.chars().count();
            if state.cursor_position < total_chars {
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.remove(byte_pos);
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        DomainKey::Left if state.focus == FocusState::Input => {
            state.cursor_position = state.cursor_position.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Right if state.focus == FocusState::Input => {
            if state.cursor_position < state.input_buffer.chars().count() {
                state.cursor_position += 1;
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        // Home: move to start of current line
        DomainKey::Home if state.focus == FocusState::Input => {
            let (row, _col) =
                input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            state.cursor_position = input_box::row_col_to_cursor(&state.input_buffer, row, 0);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // End: move to end of current line
        DomainKey::End if state.focus == FocusState::Input => {
            let (row, _col) =
                input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            let line_len = input_box::line_len_at_row(&state.input_buffer, row);
            state.cursor_position =
                input_box::row_col_to_cursor(&state.input_buffer, row, line_len);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Up/Down in Input focus: multi-line navigation or history
        // Covers: UX-DR74, UX-DR76
        DomainKey::Up if state.focus == FocusState::Input => {
            let has_multiline_content = state.input_buffer.contains('\n');
            let (row, col) =
                input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);

            if has_multiline_content && row > 0 && !state.input_history.is_navigating() {
                // Move cursor up one line (only if not already navigating history)
                let target_col = col.min(input_box::line_len_at_row(&state.input_buffer, row - 1));
                state.cursor_position =
                    input_box::row_col_to_cursor(&state.input_buffer, row - 1, target_col);
                ensure_cursor_visible(state);
                state.needs_redraw = true;
            } else if state.input_buffer.is_empty()
                || state.input_history.is_navigating()
                || !has_multiline_content
            {
                // Navigate history
                let current = state.input_buffer.clone();
                if let Some(entry) = state.input_history.navigate_up(&current) {
                    state.input_buffer = entry.to_string();
                    state.cursor_position = state.input_buffer.chars().count();
                    state.input_scroll_offset = 0;
                    ensure_cursor_visible(state);
                    state.needs_redraw = true;
                }
            }
            InputAction::Consumed
        }

        DomainKey::Down if state.focus == FocusState::Input => {
            let has_multiline_content = state.input_buffer.contains('\n');
            let (row, col) =
                input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            let total_lines = input_box::line_count(&state.input_buffer);

            if has_multiline_content
                && row + 1 < total_lines
                && !state.input_history.is_navigating()
            {
                // Move cursor down one line (only if not already navigating history)
                let target_col = col.min(input_box::line_len_at_row(&state.input_buffer, row + 1));
                state.cursor_position =
                    input_box::row_col_to_cursor(&state.input_buffer, row + 1, target_col);
                ensure_cursor_visible(state);
                state.needs_redraw = true;
            } else if state.input_history.is_navigating() {
                // Navigate history forward
                if let Some(entry) = state.input_history.navigate_down() {
                    state.input_buffer = entry.to_string();
                    state.cursor_position = state.input_buffer.chars().count();
                    state.input_scroll_offset = 0;
                    ensure_cursor_visible(state);
                    state.needs_redraw = true;
                }
            }
            InputAction::Consumed
        }

        DomainKey::Enter if matches!(state.focus, FocusState::Sidebar { .. }) => {
            // Open selected conversation — event loop resolves ID from session_index
            InputAction::OpenSidebarConversation
        }

        DomainKey::Enter if state.focus == FocusState::Chat => {
            // Toggle collapse/expand on focused tool block
            if let Some(ref tool_id) = state.focused_tool_id {
                let entry = state.tool_block_states.entry(tool_id.clone()).or_default();
                entry.collapsed = !entry.collapsed;
                entry.peek_active = false;
                state.height_cache.invalidate_all();
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }

        // Enter in Input: behavior depends on multiline_mode
        // Covers: UX-DR76
        DomainKey::Enter if state.focus == FocusState::Input => {
            if state.multiline_mode {
                // In multiline mode, Enter inserts newline (only if buffer has content)
                if !state.input_buffer.is_empty() {
                    insert_newline(state);
                }
                InputAction::Consumed
            } else if !state.input_buffer.is_empty() {
                submit_message(state)
            } else {
                InputAction::Consumed
            }
        }

        DomainKey::CtrlC => InputAction::CancelOrQuit,
        DomainKey::CtrlH => InputAction::ToggleSidebar,
        DomainKey::CtrlT => InputAction::NewTab,
        // Tab/focus cycling (AC11):
        // - Chat + sidebar visible → Sidebar
        // - Chat + no sidebar → SwitchToNextTab (existing behavior)
        // - Sidebar → Input
        DomainKey::Tab if state.focus == FocusState::Chat && state.sidebar_visible => {
            state.focus = FocusState::Sidebar {
                panel: crate::domain::models::visual::PanelType::History,
                selected: state.sidebar_selected,
            };
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Tab if state.focus == FocusState::Chat => InputAction::SwitchToNextTab,
        DomainKey::Tab if matches!(state.focus, FocusState::Sidebar { .. }) => {
            state.focus = FocusState::Input;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::ShiftTab if state.focus != FocusState::Input => InputAction::SwitchToPrevTab,
        _ => InputAction::Ignored,
    }
}

/// Insert a newline at cursor position.
// Covers: UX-DR76
fn insert_newline(state: &mut TuiState) {
    let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
    state.input_buffer.insert(byte_pos, '\n');
    state.cursor_position += 1;
    ensure_cursor_visible(state);
    state.needs_redraw = true;
}

/// Submit the current input buffer as a message.
// Covers: UX-DR77
fn submit_message(state: &mut TuiState) -> InputAction {
    let text = std::mem::take(&mut state.input_buffer);
    state.input_history.push(text.clone());
    state.input_history.reset_navigation();
    state.cursor_position = 0;
    state.input_scroll_offset = 0;
    state.autocomplete.dismiss();
    state.needs_redraw = true;

    // Check if this is a slash command
    if let Some(after_slash) = text.strip_prefix('/') {
        let cmd_name = after_slash
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        if !cmd_name.is_empty() {
            // Check if it's a built-in command
            if cmd_name == "new" {
                return InputAction::ExecuteCommand(cmd_name);
            }
            // /ml: toggle multi-line mode (AC#3)
            // Covers: Sprint Change Proposal 2026-04-08
            if cmd_name == "ml" {
                return InputAction::ExecuteCommand(cmd_name);
            }
            // User-defined command: submit with command context
            let remainder = after_slash[cmd_name.len()..].trim().to_string();
            state.resolved_mentions.clear();
            return InputAction::SubmitWithContext {
                text: remainder,
                command: Some(cmd_name),
            };
        }
    }

    // If there are resolved file mentions, use SubmitWithContext so the event loop
    // can read state.resolved_mentions before clearing them.
    if !state.resolved_mentions.is_empty() {
        return InputAction::SubmitWithContext {
            text,
            command: None,
        };
    }

    InputAction::SubmitMessage(text)
}

/// Ensure the cursor's row is visible within the input box scroll window.
fn ensure_cursor_visible(state: &mut TuiState) {
    let (cursor_row, _) = input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
    let max_visible = input_box::MAX_INPUT_LINES;
    if cursor_row >= state.input_scroll_offset + max_visible {
        state.input_scroll_offset = cursor_row + 1 - max_visible;
    } else if cursor_row < state.input_scroll_offset {
        state.input_scroll_offset = cursor_row;
    }
}

/// Handle keys while autocomplete popup is active.
// Covers: UX-DR75
fn handle_autocomplete_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        // Up/Down navigate the suggestion list
        DomainKey::Up => {
            state.autocomplete.navigate(Direction::Up);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Down => {
            state.autocomplete.navigate(Direction::Down);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Tab or Enter selects the current suggestion
        DomainKey::Tab | DomainKey::Enter => {
            if let Some(suggestion) = state.autocomplete.selected().cloned() {
                apply_autocomplete_selection(state, &suggestion);
            }
            state.autocomplete.dismiss();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Esc dismisses the popup
        DomainKey::Esc => {
            state.autocomplete.dismiss();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Backspace: if cursor goes back to/before trigger position, dismiss
        DomainKey::Backspace => {
            if state.cursor_position == state.autocomplete.trigger_position.saturating_add(1) {
                // Cursor is exactly one past trigger — remove the trigger character and dismiss
                if state.cursor_position > 0 {
                    state.cursor_position -= 1;
                    let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                    state.input_buffer.remove(byte_pos);
                }
                state.autocomplete.dismiss();
                state.needs_redraw = true;
                InputAction::Consumed
            } else if state.cursor_position <= state.autocomplete.trigger_position {
                // Cursor somehow at or before trigger — just dismiss without editing
                state.autocomplete.dismiss();
                state.needs_redraw = true;
                InputAction::Consumed
            } else {
                // Normal backspace within filter text
                if state.cursor_position > 0 {
                    state.cursor_position -= 1;
                    let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                    state.input_buffer.remove(byte_pos);
                    // Update filter text
                    let trigger = state.autocomplete.trigger_position;
                    let filter: String = state
                        .input_buffer
                        .chars()
                        .skip(trigger + 1)
                        .take(state.cursor_position.saturating_sub(trigger + 1))
                        .collect();
                    state.autocomplete.filter_text = filter;
                    state.needs_redraw = true;
                }
                InputAction::Consumed
            }
        }
        DomainKey::CtrlC => {
            state.autocomplete.dismiss();
            InputAction::CancelOrQuit
        }
        // All other special keys dismiss autocomplete and fall through to normal handling
        _ => {
            state.autocomplete.dismiss();
            state.needs_redraw = true;
            InputAction::Ignored
        }
    }
}

/// Apply the selected autocomplete suggestion to the input buffer.
fn apply_autocomplete_selection(
    state: &mut TuiState,
    suggestion: &crate::domain::models::autocomplete::AutocompleteSuggestion,
) {
    use crate::adapters::tui::state::ResolvedMention;
    use crate::domain::models::autocomplete::AutocompleteSuggestion;

    let trigger = state.autocomplete.trigger_position;

    match suggestion {
        AutocompleteSuggestion::SlashCommand { name, .. } => {
            // Replace everything from trigger to cursor with "/<name>"
            let before: String = state.input_buffer.chars().take(trigger).collect();
            let after: String = state
                .input_buffer
                .chars()
                .skip(state.cursor_position)
                .collect();
            state.input_buffer = format!("{}/{}{}", before, name, after);
            state.cursor_position = trigger + 1 + name.chars().count();
        }
        AutocompleteSuggestion::FilePath { path, .. } => {
            // Replace everything from trigger to cursor with "@<path>"
            let before: String = state.input_buffer.chars().take(trigger).collect();
            let after: String = state
                .input_buffer
                .chars()
                .skip(state.cursor_position)
                .collect();
            state.input_buffer = format!("{}@{}{}", before, path, after);
            state.cursor_position = trigger + 1 + path.chars().count();
            // Track resolved mention for file context attachment at send time (deduplicate)
            if !state.resolved_mentions.iter().any(|m| m.path == *path) {
                state
                    .resolved_mentions
                    .push(ResolvedMention { path: path.clone() });
            }
        }
    }
}

/// Handle special keys while command palette is active.
// Covers: UX-DR18
fn handle_command_palette_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Up => {
            state.command_palette.navigate(Direction::Up);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Down => {
            state.command_palette.navigate(Direction::Down);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Enter => {
            // Execute selected entry's action
            let action = state.command_palette.execute_selected();
            let prev = state.command_palette.dismiss();
            if let Some(focus) = prev {
                state.focus = focus;
            }
            state.needs_redraw = true;
            if let Some(palette_action) = action {
                return dispatch_palette_action(state, palette_action);
            }
            InputAction::Consumed
        }
        DomainKey::Esc => {
            state.focus = state.command_palette.dismiss().unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Backspace => {
            if !state.command_palette.filter_text.is_empty() {
                state.command_palette.filter_text.pop();
                // Reset scope if prefix character was deleted
                if state.command_palette.filter_text.is_empty() {
                    state.command_palette.current_scope = None;
                }
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        DomainKey::Tab => InputAction::Consumed, // No-op
        DomainKey::CtrlC => {
            let prev = state.command_palette.dismiss();
            state.focus = prev.unwrap_or(FocusState::Input);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Handle character keys while the help overlay is active.
// Covers: FR108, UX-DR94
fn handle_help_overlay_char(state: &mut TuiState, c: char) -> InputAction {
    match c {
        // j / Down → scroll toward bottom (increment offset)
        'j' => {
            state.help_overlay.scroll_offset = state.help_overlay.scroll_offset.saturating_add(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // k / Up → scroll toward top (decrement offset)
        'k' => {
            state.help_overlay.scroll_offset = state.help_overlay.scroll_offset.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // G → scroll to bottom (large sentinel; render fn clamps to max)
        'G' => {
            state.help_overlay.scroll_offset = usize::MAX / 2;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // g → scroll to top
        'g' => {
            state.help_overlay.scroll_offset = 0;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // ? → toggle off (dismiss)
        '?' => {
            state.focus = state.help_overlay.close();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        _ => InputAction::Consumed,
    }
}

/// Handle special keys while the help overlay is active.
// Covers: FR108, UX-DR94
fn handle_help_overlay_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Esc => {
            state.focus = state.help_overlay.close();
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Down => {
            state.help_overlay.scroll_offset = state.help_overlay.scroll_offset.saturating_add(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Up => {
            state.help_overlay.scroll_offset = state.help_overlay.scroll_offset.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        // Ctrl+C: pass through to cancel streaming — help overlay is passive, not interactive
        DomainKey::CtrlC => {
            state.focus = state.help_overlay.close();
            state.needs_redraw = true;
            InputAction::CancelOrQuit
        }
        _ => InputAction::Consumed,
    }
}

/// Dispatch a palette action to the appropriate handler.
fn dispatch_palette_action(
    state: &mut TuiState,
    action: crate::domain::models::palette::PaletteAction,
) -> InputAction {
    use crate::domain::models::palette::PaletteAction;

    match action {
        PaletteAction::ExecuteCommand(name) => InputAction::ExecuteCommand(name),
        PaletteAction::InsertMention(path) => {
            // Insert @path at cursor position, return to input focus
            let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
            let mention = format!("@{}", path);
            state.input_buffer.insert_str(byte_pos, &mention);
            state.cursor_position += mention.chars().count();
            state.focus = FocusState::Input;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        PaletteAction::SwitchModel(_)
        | PaletteAction::SwitchProfile(_)
        | PaletteAction::OpenPanel(_) => {
            // Stubs for future epics
            InputAction::Consumed
        }
        PaletteAction::ShowVersion => {
            // Display version info as a FeedbackBlock in the chat pane
            // Covers: FR109
            let version = crate::adapters::tui::version_info::version_string();
            let block = crate::domain::models::FeedbackBlock {
                id: "version-info".to_string(),
                level: crate::domain::models::FeedbackLevel::Info,
                message: version,
                actions: Vec::new(),
            };
            state.feedback_blocks.insert(block.id.clone(), block);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        PaletteAction::NewTab => InputAction::NewTab,
        PaletteAction::CloseTab => InputAction::CloseTab,
        PaletteAction::DeleteAllConversations => InputAction::DeleteAllConversations,
        PaletteAction::PasteImageFromClipboard => InputAction::RequestClipboardPaste,
        PaletteAction::Noop => {
            // Show "Not yet available" feedback
            let block = crate::domain::models::FeedbackBlock {
                id: "palette-noop".to_string(),
                level: crate::domain::models::FeedbackLevel::Info,
                message: "Not yet available".to_string(),
                actions: Vec::new(),
            };
            state.feedback_blocks.insert(block.id.clone(), block);
            state.needs_redraw = true;
            InputAction::Consumed
        }
    }
}

/// Handle keys while reverse search overlay is active.
// Covers: UX-DR74
fn handle_reverse_search_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Enter => {
            // Select current match and populate input
            if let Some(selected) = state
                .reverse_search
                .matches
                .get(state.reverse_search.selected_match)
            {
                state.input_buffer = selected.1.clone();
                state.cursor_position = state.input_buffer.chars().count();
            }
            state.reverse_search.active = false;
            state.focus = FocusState::Input;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Esc => {
            state.reverse_search.active = false;
            state.focus = FocusState::Input;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::Up | DomainKey::CtrlR => {
            // Cycle to next match
            if !state.reverse_search.matches.is_empty() {
                state.reverse_search.selected_match =
                    (state.reverse_search.selected_match + 1) % state.reverse_search.matches.len();
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        DomainKey::Down => {
            // Cycle to previous match
            if !state.reverse_search.matches.is_empty() {
                if state.reverse_search.selected_match == 0 {
                    state.reverse_search.selected_match = state.reverse_search.matches.len() - 1;
                } else {
                    state.reverse_search.selected_match -= 1;
                }
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        DomainKey::Backspace => {
            state.reverse_search.query.pop();
            update_reverse_search_matches(state);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainKey::CtrlC => InputAction::CancelOrQuit,
        _ => InputAction::Consumed,
    }
}

/// Update reverse search matches from current query.
fn update_reverse_search_matches(state: &mut TuiState) {
    let old_selected = state.reverse_search.selected_match;
    let results = state.input_history.search(&state.reverse_search.query);
    state.reverse_search.matches = results
        .into_iter()
        .map(|(i, s)| (i, s.to_string()))
        .collect();
    let new_len = state.reverse_search.matches.len();
    state.reverse_search.selected_match = old_selected.min(new_len.saturating_sub(1));
}

/// Convert a crossterm key event into a domain input event.
/// This is the ONLY place where crossterm types are mapped to domain types.
// Covers: FR16, UX-DR76, UX-DR74
pub fn convert_crossterm_event(event: &crossterm::event::Event) -> Option<DomainInputEvent> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    match event {
        Event::Key(KeyEvent {
            code, modifiers, ..
        }) => {
            // Ctrl+C → mapped to DomainKey::CtrlC
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('c') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlC));
            }
            // Ctrl+E → toggle multi-line mode
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('e') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlE));
            }
            // Ctrl+H → toggle history sidebar
            // Covers: FR107, UX-DR20
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('h') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlH));
            }
            // Alt+M → toggle multi-line mode (VS Code terminal alternative to Ctrl+E)
            // Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment
            if *modifiers == KeyModifiers::ALT && *code == KeyCode::Char('m') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::AltM));
            }
            // Alt+V → paste image (or text) from the system clipboard.
            // We use Alt+V rather than Ctrl+V because terminal emulators intercept
            // Ctrl+V / Ctrl+Shift+V themselves for their own paste operation.
            if *modifiers == KeyModifiers::ALT && *code == KeyCode::Char('v') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::AltV));
            }
            // Ctrl+P → command palette
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('p') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlP));
            }
            // Ctrl+R → reverse search
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('r') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlR));
            }
            // Ctrl+X → which-key chords
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('x') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlX));
            }
            // Ctrl+T → new tab
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('t') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlT));
            }

            match code {
                KeyCode::Char(c) => Some(DomainInputEvent::KeyPress(*c)),
                KeyCode::Enter => {
                    if modifiers.contains(KeyModifiers::SHIFT) {
                        Some(DomainInputEvent::SpecialKey(DomainKey::ShiftEnter))
                    } else if modifiers.contains(KeyModifiers::ALT) {
                        // Alt+Enter: VS Code terminal alternative to Shift+Enter
                        // Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment
                        Some(DomainInputEvent::SpecialKey(DomainKey::AltEnter))
                    } else if modifiers.contains(KeyModifiers::CONTROL) {
                        Some(DomainInputEvent::SpecialKey(DomainKey::CtrlEnter))
                    } else {
                        Some(DomainInputEvent::SpecialKey(DomainKey::Enter))
                    }
                }
                KeyCode::Esc => Some(DomainInputEvent::SpecialKey(DomainKey::Esc)),
                KeyCode::Backspace => Some(DomainInputEvent::SpecialKey(DomainKey::Backspace)),
                KeyCode::Delete => Some(DomainInputEvent::SpecialKey(DomainKey::Delete)),
                KeyCode::Up => Some(DomainInputEvent::SpecialKey(DomainKey::Up)),
                KeyCode::Down => Some(DomainInputEvent::SpecialKey(DomainKey::Down)),
                KeyCode::Left => Some(DomainInputEvent::SpecialKey(DomainKey::Left)),
                KeyCode::Right => Some(DomainInputEvent::SpecialKey(DomainKey::Right)),
                KeyCode::Home => Some(DomainInputEvent::SpecialKey(DomainKey::Home)),
                KeyCode::End => Some(DomainInputEvent::SpecialKey(DomainKey::End)),
                KeyCode::Tab => Some(DomainInputEvent::SpecialKey(DomainKey::Tab)),
                KeyCode::BackTab => Some(DomainInputEvent::SpecialKey(DomainKey::ShiftTab)),
                _ => None,
            }
        }
        Event::Paste(data) => {
            // Try to detect base64-encoded image data (common from xclip -o | base64).
            // If it decodes to valid image magic bytes, treat as image paste.
            use base64::Engine;
            let trimmed = data.trim();
            if !trimmed.is_empty() {
                if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
                    if crate::adapters::tui::image::detect_image_format(&decoded).is_ok() {
                        return Some(DomainInputEvent::ImagePaste(decoded));
                    }
                }
            }
            // Not an image — treat as text paste
            tracing::debug!("Paste data is not a recognized base64 image, treating as text");
            Some(DomainInputEvent::Paste(data.clone()))
        }
        Event::Resize(w, h) => Some(DomainInputEvent::Resize(*w, *h)),
        Event::FocusGained => Some(DomainInputEvent::FocusGained),
        Event::FocusLost => Some(DomainInputEvent::FocusLost),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn alt_key(c: char) -> crossterm::event::Event {
        crossterm::event::Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn ctrl_key(c: char) -> crossterm::event::Event {
        crossterm::event::Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    // ── convert_crossterm_event ──────────────────────────────────────────────

    fn make_state() -> TuiState {
        TuiState::new(80, 24)
    }

    #[test]
    fn alt_v_maps_to_alt_v_domain_key() {
        let event = alt_key('v');
        assert!(matches!(
            convert_crossterm_event(&event),
            Some(DomainInputEvent::SpecialKey(DomainKey::AltV))
        ));
    }

    #[test]
    fn alt_m_maps_to_alt_m_domain_key() {
        // Regression: ensure AltM still works after AltV addition
        let event = alt_key('m');
        assert!(matches!(
            convert_crossterm_event(&event),
            Some(DomainInputEvent::SpecialKey(DomainKey::AltM))
        ));
    }

    #[test]
    fn ctrl_v_does_not_map_to_alt_v() {
        // Ctrl+V must not accidentally trigger clipboard paste (terminals intercept it)
        let event = ctrl_key('v');
        let result = convert_crossterm_event(&event);
        assert!(!matches!(
            result,
            Some(DomainInputEvent::SpecialKey(DomainKey::AltV))
        ));
    }

    // ── handle_special_key via handle_input ─────────────────────────────────

    #[test]
    fn alt_v_returns_request_clipboard_paste_in_input_focus() {
        let mut state = make_state();
        state.focus = FocusState::Input;
        let event = DomainInputEvent::SpecialKey(DomainKey::AltV);
        let action = handle_input(&mut state, &event);
        assert_eq!(action, InputAction::RequestClipboardPaste);
    }

    #[test]
    fn alt_v_returns_request_clipboard_paste_in_chat_focus() {
        // Alt+V works regardless of focus — clipboard paste is a global action
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let event = DomainInputEvent::SpecialKey(DomainKey::AltV);
        let action = handle_input(&mut state, &event);
        assert_eq!(action, InputAction::RequestClipboardPaste);
    }

    // ── dispatch_palette_action ──────────────────────────────────────────────

    #[test]
    fn palette_paste_image_dispatches_request_clipboard_paste() {
        let mut state = make_state();
        let action = dispatch_palette_action(
            &mut state,
            crate::domain::models::palette::PaletteAction::PasteImageFromClipboard,
        );
        assert_eq!(action, InputAction::RequestClipboardPaste);
    }
}
