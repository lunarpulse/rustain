use crate::adapters::tui::state::TuiState;
use crate::domain::events::{DomainInputEvent, DomainKey};
use crate::domain::models::FocusState;

/// Handle a domain input event by updating TUI state.
/// Returns true if the event was consumed.
pub fn handle_input(state: &mut TuiState, event: &DomainInputEvent) -> bool {
    match event {
        DomainInputEvent::KeyPress(c) => handle_char(state, *c),
        DomainInputEvent::SpecialKey(key) => handle_special_key(state, *key),
        DomainInputEvent::Resize(w, h) => {
            state.terminal_width = *w;
            state.terminal_height = *h;
            state.needs_redraw = true;
            true
        }
        DomainInputEvent::FocusGained | DomainInputEvent::FocusLost => {
            state.needs_redraw = true;
            true
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

fn handle_char(state: &mut TuiState, c: char) -> bool {
    match state.focus {
        FocusState::Input => {
            let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
            state.input_buffer.insert(byte_pos, c);
            state.cursor_position += 1;
            state.needs_redraw = true;
            true
        }
        FocusState::Chat => match c {
            'i' => {
                state.focus = FocusState::Input;
                state.needs_redraw = true;
                true
            }
            'q' => {
                state.should_quit = true;
                true
            }
            _ => false,
        },
    }
}

fn handle_special_key(state: &mut TuiState, key: DomainKey) -> bool {
    match key {
        DomainKey::Esc => {
            state.focus = match state.focus {
                FocusState::Input => FocusState::Chat,
                FocusState::Chat => FocusState::Input,
            };
            state.needs_redraw = true;
            true
        }
        DomainKey::Backspace if state.focus == FocusState::Input => {
            if state.cursor_position > 0 {
                state.cursor_position -= 1;
                let byte_pos = char_to_byte(&state.input_buffer, state.cursor_position);
                state.input_buffer.remove(byte_pos);
                state.needs_redraw = true;
            }
            true
        }
        DomainKey::Left if state.focus == FocusState::Input => {
            state.cursor_position = state.cursor_position.saturating_sub(1);
            state.needs_redraw = true;
            true
        }
        DomainKey::Right if state.focus == FocusState::Input => {
            if state.cursor_position < state.input_buffer.chars().count() {
                state.cursor_position += 1;
                state.needs_redraw = true;
            }
            true
        }
        DomainKey::Enter if state.focus == FocusState::Input => {
            // Placeholder: clear input on Enter (send message in later stories)
            if !state.input_buffer.is_empty() {
                state.input_buffer.clear();
                state.cursor_position = 0;
                state.needs_redraw = true;
            }
            true
        }
        _ => false,
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
