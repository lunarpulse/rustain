use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
use crate::adapters::tui::widgets::tool_block::ToolBlockState;
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};
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

    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn invalidate_last(&mut self, message_index: usize) {
        self.entries.remove(&message_index);
    }
}

/// Session-scoped input history buffer for Up/Down navigation and Ctrl+R search.
/// Bounded to MAX_HISTORY entries. Not persisted across restarts.
// Covers: UX-DR74 (input history)
pub struct InputHistory {
    entries: VecDeque<String>,
    /// Current position when navigating (None = not navigating).
    cursor: Option<usize>,
    /// Saves current input when user starts navigating history.
    draft: String,
}

const MAX_HISTORY: usize = 100;

impl InputHistory {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            cursor: None,
            draft: String::new(),
        }
    }

    /// Add entry, evict oldest if at capacity.
    pub fn push(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        // Suppress consecutive identical entries (shell-history convention, UX-DR74)
        if self.entries.back() == Some(&text) {
            return;
        }
        self.entries.push_back(text);
        if self.entries.len() > MAX_HISTORY {
            self.entries.pop_front();
        }
        self.reset_navigation();
    }

    /// Move cursor up (toward older entries), return entry.
    /// On first call, saves current_input as draft.
    pub fn navigate_up(&mut self, current_input: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        match self.cursor {
            None => {
                // Start navigating from newest entry
                self.draft = current_input.to_string();
                let idx = self.entries.len() - 1;
                self.cursor = Some(idx);
                Some(&self.entries[idx])
            }
            Some(idx) => {
                if idx > 0 {
                    let new_idx = idx - 1;
                    self.cursor = Some(new_idx);
                    Some(&self.entries[new_idx])
                } else {
                    // Already at oldest entry
                    Some(&self.entries[0])
                }
            }
        }
    }

    /// Move cursor down (toward newer entries), return entry or None (back to draft).
    pub fn navigate_down(&mut self) -> Option<&str> {
        match self.cursor {
            None => None,
            Some(idx) => {
                if idx + 1 < self.entries.len() {
                    let new_idx = idx + 1;
                    self.cursor = Some(new_idx);
                    Some(&self.entries[new_idx])
                } else {
                    // Past the newest entry → back to draft
                    self.cursor = None;
                    Some(&self.draft)
                }
            }
        }
    }

    /// Clear cursor, discard draft.
    pub fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }

    /// Case-insensitive substring search, returns (index, entry) pairs.
    pub fn search(&self, query: &str) -> Vec<(usize, &str)> {
        if query.is_empty() {
            return Vec::new();
        }
        let lower_query = query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| entry.to_lowercase().contains(&lower_query))
            .map(|(i, entry)| (i, entry.as_str()))
            .collect()
    }

    /// Whether currently navigating history.
    pub fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }

    /// Get the saved draft.
    #[allow(dead_code)]
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Number of entries.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether history is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for InputHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// State for reverse search overlay (Ctrl+R).
// Covers: UX-DR74 (reverse search)
pub struct ReverseSearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<(usize, String)>,
    pub selected_match: usize,
}

impl ReverseSearchState {
    pub fn new() -> Self {
        Self {
            active: false,
            query: String::new(),
            matches: Vec::new(),
            selected_match: 0,
        }
    }
}

impl Default for ReverseSearchState {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolved file mention tracked from autocomplete selection.
/// Stored so we don't re-parse with regex at send time.
#[derive(Debug, Clone)]
pub struct ResolvedMention {
    /// File path relative to workspace.
    pub path: String,
}

/// State for inline autocomplete popup (/ commands and @ file mentions).
// Covers: UX-DR75
pub struct AutocompleteState {
    pub active: bool,
    pub kind: AutocompleteKind,
    /// Cursor position where the trigger character (/ or @) was typed.
    pub trigger_position: usize,
    /// Characters typed after the trigger, used for filtering.
    pub filter_text: String,
    /// Current filtered suggestions.
    pub suggestions: Vec<AutocompleteSuggestion>,
    /// Currently highlighted suggestion (0-based, wraps around).
    pub selected_index: usize,
    /// Scroll offset for long suggestion lists.
    pub scroll_offset: usize,
}

impl AutocompleteState {
    pub fn new() -> Self {
        Self {
            active: false,
            kind: AutocompleteKind::SlashCommand,
            trigger_position: 0,
            filter_text: String::new(),
            suggestions: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
        }
    }

    /// Open autocomplete with the given kind and trigger position.
    #[allow(dead_code)]
    pub fn open(&mut self, kind: AutocompleteKind, trigger_position: usize, suggestions: Vec<AutocompleteSuggestion>) {
        self.active = true;
        self.kind = kind;
        self.trigger_position = trigger_position;
        self.filter_text.clear();
        self.suggestions = suggestions;
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    /// Dismiss the autocomplete popup.
    pub fn dismiss(&mut self) {
        self.active = false;
        self.filter_text.clear();
        self.suggestions.clear();
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    /// Navigate up or down in the suggestion list (wraps around).
    pub fn navigate(&mut self, direction: Direction) {
        if self.suggestions.is_empty() {
            return;
        }
        match direction {
            Direction::Up => {
                if self.selected_index == 0 {
                    self.selected_index = self.suggestions.len() - 1;
                } else {
                    self.selected_index -= 1;
                }
            }
            Direction::Down => {
                self.selected_index = (self.selected_index + 1) % self.suggestions.len();
            }
        }
        // Adjust scroll offset to keep selected item visible
        let max_visible = 8;
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + max_visible {
            self.scroll_offset = self.selected_index + 1 - max_visible;
        }
    }

    /// Update the filter text and reset selection.
    #[allow(dead_code)]
    pub fn update_filter(&mut self, filter: String, suggestions: Vec<AutocompleteSuggestion>) {
        self.filter_text = filter;
        self.suggestions = suggestions;
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    /// Get the currently selected suggestion, if any.
    pub fn selected(&self) -> Option<&AutocompleteSuggestion> {
        self.suggestions.get(self.selected_index)
    }
}

impl Default for AutocompleteState {
    fn default() -> Self {
        Self::new()
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
    /// Session-scoped input history for Up/Down and Ctrl+R.
    // Covers: UX-DR74
    pub input_history: InputHistory,
    /// Whether multi-line mode is active (toggled by Ctrl+E).
    // Covers: UX-DR76
    pub multiline_mode: bool,
    /// State for reverse search overlay (Ctrl+R).
    // Covers: UX-DR74
    pub reverse_search: ReverseSearchState,
    /// Vertical scroll offset within the input box when lines exceed max height.
    pub input_scroll_offset: usize,
    /// Autocomplete popup state (/ commands and @ file mentions).
    // Covers: UX-DR75
    pub autocomplete: AutocompleteState,
    /// Resolved file mentions from autocomplete selections in the current input.
    /// Cleared on submit. Used at send time to attach file context.
    pub resolved_mentions: Vec<ResolvedMention>,
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
            input_history: InputHistory::new(),
            multiline_mode: false,
            reverse_search: ReverseSearchState::new(),
            input_scroll_offset: 0,
            autocomplete: AutocompleteState::new(),
            resolved_mentions: Vec::new(),
        }
    }
}
