use crate::domain::models::FocusState;

use super::color_detect::ColorCapability;
use super::theme::Theme;

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
    pub theme: Theme,
    pub auto_scroll: bool,
    pub scroll_offset: usize,
    pub total_content_height: usize,
}

impl TuiState {
    #[allow(dead_code)]
    pub fn new(width: u16, height: u16) -> Self {
        Self::with_capability(width, height, ColorCapability::TrueColor)
    }

    pub fn with_capability(width: u16, height: u16, capability: ColorCapability) -> Self {
        Self {
            focus: FocusState::Input,
            needs_redraw: true,
            terminal_width: width,
            terminal_height: height,
            input_buffer: String::new(),
            cursor_position: 0,
            status_message: "idle".to_string(),
            should_quit: false,
            theme: Theme::for_capability(capability),
            auto_scroll: true,
            scroll_offset: 0,
            total_content_height: 0,
        }
    }
}
