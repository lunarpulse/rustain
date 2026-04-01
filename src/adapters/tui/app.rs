use crate::adapters::tui::state::TuiState;
use crate::domain::events::{DomainInputEvent, DomainKey};
use crate::domain::models::FocusState;

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
}

/// Handle a domain input event by updating TUI state.
/// Returns an InputAction telling the event loop what to do.
pub fn handle_input(state: &mut TuiState, event: &DomainInputEvent) -> InputAction {
    match event {
        DomainInputEvent::KeyPress(c) => handle_char(state, *c),
        DomainInputEvent::SpecialKey(key) => handle_special_key(state, *key),
        DomainInputEvent::Resize(w, h) => {
            state.terminal_width = *w;
            state.terminal_height = *h;
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
    match state.focus {
        FocusState::Input => {
            let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
            state.input_buffer.insert(byte_pos, c);
            state.cursor_position += 1;
            state.needs_redraw = true;
            InputAction::Consumed
        }
        FocusState::Chat => match c {
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
            _ => InputAction::Ignored,
        },
        FocusState::Sidebar { .. } | FocusState::Overlay(_) => InputAction::Ignored,
    }
}

fn handle_special_key(state: &mut TuiState, key: DomainKey) -> InputAction {
    match key {
        DomainKey::Esc => {
            state.focus = match state.focus {
                FocusState::Input => FocusState::Chat,
                FocusState::Chat => FocusState::Input,
                FocusState::Sidebar { .. } | FocusState::Overlay(_) => FocusState::Input,
            };
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
        DomainKey::Enter if state.focus == FocusState::Input => {
            if !state.input_buffer.is_empty() {
                let text = std::mem::take(&mut state.input_buffer);
                state.cursor_position = 0;
                state.needs_redraw = true;
                InputAction::SubmitMessage(text)
            } else {
                InputAction::Consumed
            }
        }
        _ => InputAction::Ignored,
    }
}

/// Convert a crossterm key event into a domain input event.
/// This is the ONLY place where crossterm types are mapped to domain types.
pub fn convert_crossterm_event(event: &crossterm::event::Event) -> Option<DomainInputEvent> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    match event {
        Event::Key(KeyEvent {
            code, modifiers, ..
        }) => {
            // Ctrl+C → Shutdown (handled separately in event loop)
            if *modifiers == KeyModifiers::CONTROL && *code == KeyCode::Char('c') {
                return None; // Event loop handles this directly
            }

            match code {
                KeyCode::Char(c) => Some(DomainInputEvent::KeyPress(*c)),
                KeyCode::Enter => Some(DomainInputEvent::SpecialKey(DomainKey::Enter)),
                KeyCode::Esc => Some(DomainInputEvent::SpecialKey(DomainKey::Esc)),
                KeyCode::Backspace => Some(DomainInputEvent::SpecialKey(DomainKey::Backspace)),
                KeyCode::Up => Some(DomainInputEvent::SpecialKey(DomainKey::Up)),
                KeyCode::Down => Some(DomainInputEvent::SpecialKey(DomainKey::Down)),
                KeyCode::Left => Some(DomainInputEvent::SpecialKey(DomainKey::Left)),
                KeyCode::Right => Some(DomainInputEvent::SpecialKey(DomainKey::Right)),
                KeyCode::Tab => Some(DomainInputEvent::SpecialKey(DomainKey::Tab)),
                _ => None,
            }
        }
        Event::Resize(w, h) => Some(DomainInputEvent::Resize(*w, *h)),
        Event::FocusGained => Some(DomainInputEvent::FocusGained),
        Event::FocusLost => Some(DomainInputEvent::FocusLost),
        _ => None,
    }
}
