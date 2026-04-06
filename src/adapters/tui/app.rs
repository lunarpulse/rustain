use crate::adapters::tui::state::{Direction, TuiState};
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
            state.needs_redraw = true;
            InputAction::Consumed
        }
        DomainInputEvent::FocusGained | DomainInputEvent::FocusLost => {
            state.needs_redraw = true;
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
            _ => InputAction::Ignored,
        },
        FocusState::Sidebar { .. } | FocusState::Overlay(_) => InputAction::Ignored,
    }
}

fn handle_special_key(state: &mut TuiState, key: DomainKey) -> InputAction {
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
    if state.reverse_search.active {
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
                FocusState::Sidebar { .. } | FocusState::Overlay(_) => FocusState::Input,
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

        // Ctrl+Enter: submit in multiline mode
        // Covers: UX-DR76
        DomainKey::CtrlEnter if state.focus == FocusState::Input => {
            if !state.input_buffer.is_empty() {
                submit_message(state)
            } else {
                InputAction::Consumed
            }
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
            let (row, _col) = input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            state.cursor_position = input_box::row_col_to_cursor(&state.input_buffer, row, 0);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // End: move to end of current line
        DomainKey::End if state.focus == FocusState::Input => {
            let (row, _col) = input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            let line_len = input_box::line_len_at_row(&state.input_buffer, row);
            state.cursor_position = input_box::row_col_to_cursor(&state.input_buffer, row, line_len);
            state.needs_redraw = true;
            InputAction::Consumed
        }

        // Up/Down in Input focus: multi-line navigation or history
        // Covers: UX-DR74, UX-DR76
        DomainKey::Up if state.focus == FocusState::Input => {
            let has_multiline_content = state.input_buffer.contains('\n');
            let (row, col) = input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);

            if has_multiline_content && row > 0 && !state.input_history.is_navigating() {
                // Move cursor up one line (only if not already navigating history)
                let target_col = col.min(input_box::line_len_at_row(&state.input_buffer, row - 1));
                state.cursor_position = input_box::row_col_to_cursor(&state.input_buffer, row - 1, target_col);
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
            let (row, col) = input_box::cursor_to_row_col(&state.input_buffer, state.cursor_position);
            let total_lines = input_box::line_count(&state.input_buffer);

            if has_multiline_content && row + 1 < total_lines && !state.input_history.is_navigating() {
                // Move cursor down one line (only if not already navigating history)
                let target_col = col.min(input_box::line_len_at_row(&state.input_buffer, row + 1));
                state.cursor_position = input_box::row_col_to_cursor(&state.input_buffer, row + 1, target_col);
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
        let cmd_name = after_slash.split_whitespace().next().unwrap_or("").to_string();
        if !cmd_name.is_empty() {
            // Check if it's a built-in command
            if cmd_name == "new" {
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
            let after: String = state.input_buffer.chars().skip(state.cursor_position).collect();
            state.input_buffer = format!("{}/{}{}", before, name, after);
            state.cursor_position = trigger + 1 + name.chars().count();
        }
        AutocompleteSuggestion::FilePath { path, .. } => {
            // Replace everything from trigger to cursor with "@<path>"
            let before: String = state.input_buffer.chars().take(trigger).collect();
            let after: String = state.input_buffer.chars().skip(state.cursor_position).collect();
            state.input_buffer = format!("{}@{}{}", before, path, after);
            state.cursor_position = trigger + 1 + path.chars().count();
            // Track resolved mention for file context attachment at send time (deduplicate)
            if !state.resolved_mentions.iter().any(|m| m.path == *path) {
                state.resolved_mentions.push(ResolvedMention {
                    path: path.clone(),
                });
            }
        }
    }
}

/// Handle keys while reverse search overlay is active.
// Covers: UX-DR74
fn handle_reverse_search_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Enter => {
            // Select current match and populate input
            if let Some(selected) = state.reverse_search.matches.get(state.reverse_search.selected_match) {
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
    state.reverse_search.matches = results.into_iter().map(|(i, s)| (i, s.to_string())).collect();
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
            // Ctrl+R → reverse search
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('r') {
                return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlR));
            }

            match code {
                KeyCode::Char(c) => Some(DomainInputEvent::KeyPress(*c)),
                KeyCode::Enter => {
                    if modifiers.contains(KeyModifiers::SHIFT) {
                        Some(DomainInputEvent::SpecialKey(DomainKey::ShiftEnter))
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
        Event::Resize(w, h) => Some(DomainInputEvent::Resize(*w, *h)),
        Event::FocusGained => Some(DomainInputEvent::FocusGained),
        Event::FocusLost => Some(DomainInputEvent::FocusLost),
        _ => None,
    }
}
