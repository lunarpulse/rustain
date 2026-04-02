use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
use crate::adapters::tui::widgets::tool_block::ToolBlockState;
use crate::domain::models::{
    ApprovalDecision, FeedbackBlock, FocusState, RetryState, StatusState, UsageInfo,
};

use super::color_detect::ColorCapability;
use super::theme::Theme;

/// Pending permission request awaiting user response.
pub struct PendingPermission {
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub response_tx: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

/// Queue for permission requests that arrive while another is being displayed.
#[derive(Default)]
pub struct PermissionQueue {
    queue: VecDeque<PendingPermission>,
}

impl PermissionQueue {
    pub fn push(&mut self, p: PendingPermission) {
        self.queue.push_back(p);
    }

    pub fn pop(&mut self) -> Option<PendingPermission> {
        self.queue.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// Direction for boundary navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// Cache of rendered line heights for each message/block at current terminal width.
/// Key: (message_index, block_index). Value: rendered line count.
/// block_index 0 = role line + spacing, block_index 1+ = content blocks.
/// For simplicity in MVP, we cache per-message (block_index always 0).
#[derive(Debug, Default)]
pub struct HeightCache {
    entries: HashMap<usize, usize>,
    /// Terminal width at which heights were computed.
    pub cached_width: u16,
}

impl HeightCache {
    /// Get cached height for a message index.
    pub fn get(&self, message_index: usize) -> Option<usize> {
        self.entries.get(&message_index).copied()
    }

    /// Set cached height for a message index.
    pub fn set(&mut self, message_index: usize, height: usize) {
        self.entries.insert(message_index, height);
    }

    /// Full invalidation (e.g., on resize).
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
    }

    /// Incremental invalidation: only invalidate the last message (streaming).
    pub fn invalidate_last(&mut self, message_index: usize) {
        self.entries.remove(&message_index);
    }
}

/// TUI-specific state for rendering.
pub struct TuiState {
    pub focus: FocusState,
    pub needs_redraw: bool,
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub input_buffer: String,
    pub cursor_position: usize,
    pub status: StatusState,
    /// Cumulative token usage for the current session.
    pub token_usage: Option<UsageInfo>,
    /// Previous status state for flash message revert.
    pub status_before_flash: Option<StatusState>,
    pub should_quit: bool,
    pub theme: Theme,
    pub auto_scroll: bool,
    pub scroll_offset: usize,
    pub total_content_height: usize,
    /// Line offsets for each content block boundary in rendered view.
    pub block_boundaries: Vec<usize>,
    /// Line offsets for each user message boundary in rendered view.
    pub message_boundaries: Vec<usize>,
    /// Height cache for virtual scrolling.
    pub height_cache: HeightCache,
    /// Pending anchor message index for resize scroll preservation.
    /// Set by resize handler, consumed by next render to recompute scroll_offset.
    pub pending_anchor: Option<usize>,
    /// Per-tool-block collapsed/expanded/peek state, keyed by tool_use_id.
    pub tool_block_states: HashMap<String, ToolBlockState>,
    /// Currently focused tool block id (set by chat pane render when a tool block
    /// is at the top of the viewport after J/K navigation).
    pub focused_tool_id: Option<String>,
    /// Pending permission request awaiting user y/n/a response.
    pub pending_permission: Option<PendingPermission>,
    /// Queue for additional permission requests that arrive while one is displayed.
    pub permission_queue: PermissionQueue,
    /// Active retry state for provider error recovery.
    pub retry_state: Option<RetryState>,
    /// Feedback blocks displayed in conversation, keyed by block ID.
    pub feedback_blocks: BTreeMap<String, FeedbackBlock>,
    /// The ID of the most recent active (actionable) feedback block.
    pub active_feedback_id: Option<String>,
    /// Active AskUserQuestion card state.
    pub ask_user_question: Option<AskUserQuestionState>,
    /// Oneshot sender for AskUserQuestion responses.
    pub question_response_tx: Option<tokio::sync::oneshot::Sender<String>>,
    /// Whether project context files were loaded (for status bar indicator).
    pub has_project_context: bool,
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
            status: StatusState::Idle,
            token_usage: None,
            status_before_flash: None,
            should_quit: false,
            theme: Theme::for_capability(capability),
            auto_scroll: true,
            scroll_offset: 0,
            total_content_height: 0,
            block_boundaries: Vec::new(),
            message_boundaries: Vec::new(),
            height_cache: HeightCache::default(),
            pending_anchor: None,
            tool_block_states: HashMap::new(),
            focused_tool_id: None,
            pending_permission: None,
            permission_queue: PermissionQueue::default(),
            retry_state: None,
            feedback_blocks: BTreeMap::new(),
            active_feedback_id: None,
            ask_user_question: None,
            question_response_tx: None,
            has_project_context: false,
        }
    }
}
