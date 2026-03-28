use crate::domain::models::FocusState;

/// TUI-specific state for rendering.
pub struct TuiState {
    pub focus: FocusState,
    pub needs_redraw: bool,
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub input_buffer: String,
    pub cursor_position: usize,
    pub status_message: String,
    pub should_quit: bool,
}

impl TuiState {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            focus: FocusState::Input,
            needs_redraw: true,
            terminal_width: width,
            terminal_height: height,
            input_buffer: String::new(),
            cursor_position: 0,
            status_message: "idle".to_string(),
            should_quit: false,
        }
    }
}
