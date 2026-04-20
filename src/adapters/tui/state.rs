use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

use crate::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
use crate::adapters::tui::widgets::tool_block::ToolBlockState;
use crate::domain::models::ImageAttachment;
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};
use crate::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use crate::domain::models::{
    ApprovalDecision, FeedbackBlock, FocusState, RetryState, StatusState, UsageInfo,
};
use crate::domain::services::cross_search::CrossSearchResult;
use crate::domain::services::search::SearchMatch;

use super::color_detect::ColorCapability;
use super::theme::Theme;

/// Pending permission request awaiting user response.
pub struct PendingPermission {
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub response_tx: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

/// State for the permission feedback mini-input (AC5).
pub struct FeedbackInputState {
    pub buffer: String,
    pub cursor: usize,
    pub pending_permission: PendingPermission,
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

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Drain all queued requests matching the given tool_name (AC4 batch sweep).
    /// Returns the drained requests for auto-responding.
    pub fn drain_matching(&mut self, tool_name: &str) -> Vec<PendingPermission> {
        let (matching, remaining): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|p| p.tool_name == tool_name);
        self.queue = remaining;
        matching.into_iter().collect()
    }
}

/// Direction for boundary navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// A single file entry in the rewind confirmation preview.
#[derive(Debug, Clone)]
pub struct RevertPreviewItem {
    /// Workspace-relative path for display (falls back to absolute if outside workspace).
    pub display_path: String,
    /// Whether the file has been externally modified since the snapshot was taken.
    pub conflict: bool,
}

/// Pre-flight preview data for the rewind confirmation card.
/// Built by `event_loop.rs::build_rewind_preview` without mutating any state.
#[derive(Debug, Clone)]
pub struct RewindPreview {
    /// The message index the conversation will be truncated to (inclusive).
    pub target_message_index: usize,
    /// How many messages come after target_message_index (will be removed).
    pub messages_to_remove: usize,
    /// Files that would be reverted, with conflict flags.
    pub files_to_revert: Vec<RevertPreviewItem>,
}

/// Cache of rendered line heights for each message, keyed by message ID (not index).
/// Value: rendered line count.
///
/// # Positional invalidation invariant
///
/// `truncate_from(message_index)` drops all entries at or after `message_index`.
/// This is correct for rewind (truncate-only). In-place message editing would require
/// keying by message ID alone; that is deferred until whatever future story introduces
/// edit semantics. (DF-005 resolved in 4-3b.)
#[derive(Debug, Default)]
pub struct HeightCache {
    entries: HashMap<String, usize>,
    /// Insertion-order tracking for positional invalidation via `truncate_from`.
    /// Each push in `set` appends the message ID so index == insertion order.
    id_order: Vec<String>,
    /// Terminal width at which heights were computed.
    pub cached_width: u16,
}

impl HeightCache {
    /// Get cached height for a message ID.
    pub fn get(&self, message_id: &str) -> Option<usize> {
        self.entries.get(message_id).copied()
    }

    /// Set cached height for a message ID.
    pub fn set(&mut self, message_id: String, height: usize) {
        if !self.entries.contains_key(&message_id) {
            self.id_order.push(message_id.clone());
        }
        self.entries.insert(message_id, height);
    }

    /// Full invalidation (e.g., on resize).
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
        self.id_order.clear();
    }

    /// Positional invalidation: drop all cache entries at or after `message_index`.
    ///
    /// This is safe for rewind (which only truncates) because the positional order
    /// of retained messages 0..message_index does not change. See the struct-level
    /// comment for the full invariant.
    pub fn truncate_from(&mut self, message_index: usize) {
        if message_index >= self.id_order.len() {
            return;
        }
        // Remove entries for all messages from message_index onward.
        let ids_to_remove = self.id_order.split_off(message_index);
        for id in ids_to_remove {
            self.entries.remove(&id);
        }
    }

    /// Incremental invalidation: only invalidate the given message ID (streaming).
    #[allow(dead_code)]
    pub fn invalidate_last(&mut self, message_id: &str) {
        self.entries.remove(message_id);
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

/// Sub-state of the within-conversation search overlay.
///
/// Resolves the `n` / `N` input-vs-navigation collision (Story 4-4 AC3): in
/// `Typing` every printable key builds the query; in `Navigating` the query is
/// committed and `n` / `N` cycle matches. Any printable key in `Navigating`
/// returns to `Typing` and applies the keystroke, letting the user refine the
/// query without pressing Esc.
// Covers: UX-DR86, Story 4-4 AC3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSubstate {
    Typing,
    Navigating,
}

/// Transient peek-highlight applied to a single match for 1500 ms when a
/// cross-conversation search result is opened in a new tab (Story 4-4 AC6).
/// Fields are populated by the cross-search-result handler in Task 8.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PeekHighlight {
    pub m: SearchMatch,
    pub expires_at: Instant,
}

/// State for the within-conversation search overlay (Ctrl+F).
///
/// Distinct from `ReverseSearchState` (Ctrl+R input history search) — that one
/// searches the user's past-input buffer; this one searches the rendered
/// conversation. Byte offsets in `matches` are into the **original** message
/// content string, not a lowercased copy — see `domain::services::search`.
// Covers: UX-DR86, Story 4-4 AC1-AC7
pub struct SearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<SearchMatch>,
    /// Index into `matches` of the currently focused match.
    pub focused_match_index: usize,
    /// Typing (cursor in input, n/N build query) vs Navigating (query committed,
    /// n/N cycle matches). See AC3 amendment.
    pub substate: SearchSubstate,
    /// Last time `find_matches` ran — drives the 30 ms recomputation debounce
    /// in Task 3.3, separate from the calm-jump rule.
    pub last_search_instant: Option<Instant>,
    /// Query length at the time of the last `find_matches` call — when the
    /// length changes, bypass the 30 ms debounce; when it doesn't (held-key
    /// repeat), honor the debounce and skip the scan.
    pub last_query_len: usize,
    /// Focus to restore on Esc (typically `FocusState::Chat`).
    pub prior_focus: Option<FocusState>,
    /// Transient peek highlight for cross-search result jumps (AC6).
    /// Populated by Task 8 (open cross-search result in new tab).
    #[allow(dead_code)]
    pub peek_highlight: Option<PeekHighlight>,
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            active: false,
            query: String::new(),
            matches: Vec::new(),
            focused_match_index: 0,
            substate: SearchSubstate::Typing,
            last_search_instant: None,
            last_query_len: 0,
            prior_focus: None,
            peek_highlight: None,
        }
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for the cross-conversation search overlay (`/` in sidebar focus).
///
/// Lives in the adapter layer because it's TUI view state. The actual scan
/// runs in the event loop via `domain::services::cross_search::run_cross_search`
/// and writes its results back into this struct via a stale-guarded update.
// Covers: Story 4-4 AC5, AC6, AC7 (UX-DR87)
pub struct CrossSearchState {
    pub active: bool,
    pub query: String,
    pub results: Vec<CrossSearchResult>,
    pub selected: usize,
    /// Set when the last scan hit the count cap (20 results).
    pub truncated_by_count: bool,
    /// Set when the last scan hit the wall-clock budget (200 ms).
    pub truncated_by_time: bool,
    /// Total conversations in the index at the time of the last scan.
    pub total: usize,
    /// Actual conversations scanned before truncation.
    pub scanned: usize,
    /// Currently running — shown as a "Searching…" hint while the scan task
    /// is in flight. Mirrors reviewer Fix 5's loading indicator requirement.
    pub running: bool,
}

impl CrossSearchState {
    pub fn new() -> Self {
        Self {
            active: false,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            truncated_by_count: false,
            truncated_by_time: false,
            total: 0,
            scanned: 0,
            running: false,
        }
    }
}

impl Default for CrossSearchState {
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
    pub fn open(
        &mut self,
        kind: AutocompleteKind,
        trigger_position: usize,
        suggestions: Vec<AutocompleteSuggestion>,
    ) {
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

/// Action dispatched by a which-key chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordAction {
    /// Open a panel (stub for future epics).
    #[allow(dead_code)]
    OpenPanel(crate::domain::models::visual::PanelType),
    /// Show help overlay.
    #[allow(dead_code)]
    ShowHelp,
    /// Not yet implemented — show feedback.
    Noop(String),
}

/// State for the command palette overlay (Ctrl+P).
// Covers: UX-DR18
pub struct CommandPaletteState {
    pub active: bool,
    pub filter_text: String,
    pub filtered_entries: Vec<PaletteEntry>,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub current_scope: Option<PaletteScope>,
    /// Previous focus state to restore on dismiss.
    pub previous_focus: Option<FocusState>,
}

impl CommandPaletteState {
    pub fn new() -> Self {
        Self {
            active: false,
            filter_text: String::new(),
            filtered_entries: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            current_scope: None,
            previous_focus: None,
        }
    }

    /// Open the command palette, saving the current focus for restoration.
    pub fn open(&mut self, current_focus: FocusState) {
        self.active = true;
        self.filter_text.clear();
        self.filtered_entries.clear();
        self.selected_index = 0;
        self.scroll_offset = 0;
        self.current_scope = None;
        self.previous_focus = Some(current_focus);
    }

    /// Dismiss the palette, returning the previous focus to restore.
    pub fn dismiss(&mut self) -> Option<FocusState> {
        self.active = false;
        self.filter_text.clear();
        self.filtered_entries.clear();
        self.selected_index = 0;
        self.scroll_offset = 0;
        self.current_scope = None;
        self.previous_focus.take()
    }

    /// Navigate up or down in the result list (wraps around).
    pub fn navigate(&mut self, direction: Direction) {
        if self.filtered_entries.is_empty() {
            return;
        }
        match direction {
            Direction::Up => {
                if self.selected_index == 0 {
                    self.selected_index = self.filtered_entries.len() - 1;
                } else {
                    self.selected_index -= 1;
                }
            }
            Direction::Down => {
                self.selected_index = (self.selected_index + 1) % self.filtered_entries.len();
            }
        }
        // Keep selected item visible
        let max_visible = 12;
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + max_visible {
            self.scroll_offset = self.selected_index + 1 - max_visible;
        }
    }

    /// Update filter text and refresh filtered entries from the registry.
    #[allow(dead_code)]
    pub fn update_filter(&mut self, filter: String, entries: Vec<PaletteEntry>) {
        self.filter_text = filter;
        self.filtered_entries = entries;
        self.selected_index = 0;
        self.scroll_offset = 0;
    }

    /// Get the currently selected entry, if any.
    pub fn selected(&self) -> Option<&PaletteEntry> {
        self.filtered_entries.get(self.selected_index)
    }

    /// Execute the selected entry's action. Returns the action if an entry is selected.
    pub fn execute_selected(&self) -> Option<PaletteAction> {
        self.selected().map(|entry| entry.action.clone())
    }
}

impl Default for CommandPaletteState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for the help overlay (? key or Ctrl+X, ?).
// Covers: FR108, UX-DR94
pub struct HelpOverlayState {
    pub active: bool,
    /// Focus state to restore when the overlay is dismissed.
    pub prior_focus: FocusState,
    /// Vertical scroll offset within the overlay content.
    pub scroll_offset: usize,
    /// Index of the currently highlighted category (reserved for future navigation).
    pub selected_category: usize,
}

impl HelpOverlayState {
    pub fn new() -> Self {
        Self {
            active: false,
            prior_focus: FocusState::Input,
            scroll_offset: 0,
            selected_category: 0,
        }
    }

    /// Open the help overlay, saving `prior` focus for restoration.
    pub fn open(&mut self, prior: FocusState) {
        self.active = true;
        self.prior_focus = prior;
        self.scroll_offset = 0;
        self.selected_category = 0;
    }

    /// Dismiss the overlay; returns the saved prior focus state.
    pub fn close(&mut self) -> FocusState {
        self.active = false;
        self.prior_focus.clone()
    }
}

impl Default for HelpOverlayState {
    fn default() -> Self {
        Self::new()
    }
}

/// State for the which-key hint bar overlay (Ctrl+X).
// Covers: UX-DR19, UX-DR60
pub struct WhichKeyState {
    pub active: bool,
    pub started_at: Option<Instant>,
    /// Previous focus state to restore on dismiss.
    pub previous_focus: Option<FocusState>,
    /// Chord map: key → action.
    pub chord_map: HashMap<char, ChordAction>,
}

impl WhichKeyState {
    pub fn new() -> Self {
        let mut chord_map = HashMap::new();
        chord_map.insert('p', ChordAction::Noop("Profile panel — Epic 8".to_string()));
        chord_map.insert(
            'm',
            ChordAction::Noop("Model selector — Epic 7".to_string()),
        );
        chord_map.insert('a', ChordAction::Noop("Adapter panel — Epic 8".to_string()));
        chord_map.insert(
            's',
            ChordAction::Noop("Subagent panel — Epic 10".to_string()),
        );
        chord_map.insert('l', ChordAction::Noop("Log panel — Epic 14".to_string()));
        chord_map.insert('t', ChordAction::Noop("Task panel — Epic 6".to_string()));
        chord_map.insert('u', ChordAction::Noop("Usage/cost — Epic 7".to_string()));
        chord_map.insert('w', ChordAction::Noop("Watch/monitor — future".to_string()));
        chord_map.insert('d', ChordAction::Noop("Dashboard — future".to_string()));
        chord_map.insert('?', ChordAction::ShowHelp);

        Self {
            active: false,
            started_at: None,
            previous_focus: None,
            chord_map,
        }
    }

    /// Open the which-key hint bar, saving the current focus.
    pub fn open(&mut self, current_focus: FocusState) {
        self.active = true;
        self.started_at = Some(Instant::now());
        self.previous_focus = Some(current_focus);
    }

    /// Dismiss the which-key bar, returning the previous focus.
    pub fn dismiss(&mut self) -> Option<FocusState> {
        self.active = false;
        self.started_at = None;
        self.previous_focus.take()
    }

    /// Check if the timeout has expired.
    /// A timeout_ms of 0 is treated as "no timeout" (never expires) to prevent
    /// accidental immediate expiry when callers pass 0.
    pub fn is_timed_out(&self, timeout_ms: u64) -> bool {
        if timeout_ms == 0 {
            return false;
        }
        if let Some(started) = self.started_at {
            started.elapsed().as_millis() as u64 >= timeout_ms
        } else {
            false
        }
    }

    /// Look up a chord key. Returns Some(action) for valid keys, None for invalid.
    pub fn lookup_chord(&self, key: char) -> Option<&ChordAction> {
        self.chord_map.get(&key.to_ascii_lowercase())
    }
}

impl Default for WhichKeyState {
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
    /// Height of the chat pane viewport (set after each render).
    /// Used for scroll clamping, message targeting, and status bar display.
    pub viewport_height: u16,
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
    /// Line offsets for each message boundary (all roles) in rendered view.
    /// Drives the status-bar position counter and rewind/fork targeting.
    pub message_boundaries: Vec<usize>,
    /// Line offsets for each **user** message boundary in rendered view.
    /// Drives `{`/`}` jump-between-turn navigation.
    pub user_message_boundaries: Vec<usize>,
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
    /// Pending permission request awaiting user y/n/a/s/f response.
    pub pending_permission: Option<PendingPermission>,
    /// Queue for additional permission requests that arrive while one is displayed.
    pub permission_queue: PermissionQueue,
    /// Active feedback input state (AC5 — deny with feedback mini-input).
    pub pending_feedback_input: Option<FeedbackInputState>,
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
    /// State for the within-conversation search overlay (Ctrl+F).
    // Covers: UX-DR86, Story 4-4 AC1-AC7
    pub search_state: SearchState,
    /// State for the cross-conversation search overlay (`/` in sidebar).
    // Covers: UX-DR87, Story 4-4 AC5-AC7
    pub cross_search: CrossSearchState,
    /// Vertical scroll offset within the input box when lines exceed max height.
    pub input_scroll_offset: usize,
    /// Autocomplete popup state (/ commands and @ file mentions).
    // Covers: UX-DR75
    pub autocomplete: AutocompleteState,
    /// Resolved file mentions from autocomplete selections in the current input.
    /// Cleared on submit. Used at send time to attach file context.
    pub resolved_mentions: Vec<ResolvedMention>,
    /// Images queued for the next message submission (clipboard paste or file mention).
    // Covers: FR112
    pub pending_images: Vec<ImageAttachment>,
    /// Large image awaiting user confirmation before attachment (AC4).
    // Covers: FR112
    pub pending_large_image: Option<ImageAttachment>,
    /// Visual indicator for attached images shown in the input box.
    // Covers: FR112
    pub image_indicator: Option<String>,
    /// Command palette state (Ctrl+P).
    // Covers: UX-DR18
    pub command_palette: CommandPaletteState,
    /// Which-key hint bar state (Ctrl+X).
    // Covers: UX-DR19, UX-DR60
    pub which_key: WhichKeyState,
    /// Help overlay state (? key or Ctrl+X, ?).
    // Covers: FR108, UX-DR94
    pub help_overlay: HelpOverlayState,
    /// Cached multiplexer detection (tmux/screen) — set once at startup.
    // Covers: UX-DR62
    pub multiplexer_detected: bool,
    /// Whether the terminal is VS Code's integrated terminal — set once at startup.
    // Covers: Sprint Change Proposal 2026-04-08, AC#4, AC9 (3-6a-D2)
    pub is_vscode: bool,
    /// How many TUI sessions this user has started (loaded from global config).
    /// Used to fade contextual hints after the first N sessions.
    // Covers: UX-DR96
    pub session_count: u32,
    /// Current contextual status-bar hint for discoverability.
    /// `None` once session_count > theme.timing.status_hint_fade_sessions.
    // Covers: UX-DR93
    pub current_hint: Option<String>,
    /// Whether the history sidebar is visible.
    // Covers: FR107, UX-DR20
    pub sidebar_visible: bool,
    /// Which sidebar panel is active (History, Tasks, etc.).
    // Covers: FR107, UX-DR20
    pub sidebar_panel: Option<crate::domain::models::visual::PanelType>,
    /// Selected index in the sidebar list.
    pub sidebar_selected: usize,
    /// Total number of entries in the sidebar (kept in sync by event loop).
    pub sidebar_entry_count: usize,
    /// Scroll offset for the sidebar list.
    pub sidebar_scroll_offset: usize,
    /// Pending delete confirmation target. None = no pending confirmation.
    pub pending_delete: Option<crate::domain::models::visual::DeleteConfirmTarget>,
    /// Message index selected for fork (set when user presses `f`, cleared on confirm/cancel).
    /// Story 4-3a, AC1.
    pub pending_fork_index: Option<usize>,
    /// Message index selected for rewind (set when user presses `R`, cleared on confirm/cancel).
    /// Mirrors pending_fork_index — both are session-level overlay state.
    /// Story 4-3b, AC1.
    pub pending_rewind_index: Option<usize>,
    /// Pre-computed preview data for the rewind confirmation card.
    /// Populated when pending_rewind_index is set; cleared on confirm/cancel.
    /// Story 4-3b, AC1.
    pub rewind_preview: Option<RewindPreview>,
    /// Currently selected row in the bookmark list panel (AC10). `0` when
    /// the panel is closed.
    // Covers: Story 4-4 AC10
    pub bookmark_list_selected: usize,
    /// Mirror of the active tab's bookmark count — kept in sync by the event
    /// loop whenever the panel opens or a bookmark is added / removed. The
    /// key-handler layer in `app.rs` reads this to clamp `j`/`Down` to the
    /// upper bound (Story 4-4 AC10 clamping requirement).
    pub bookmark_list_count: usize,
    /// Single-entry undo buffer for bookmark list deletes (AC10 synthesis).
    /// Stores `(deleted_index, timestamp)`; `u` within 5 s restores it, then
    /// the buffer clears silently on expiry.
    // Covers: Story 4-4 AC10 amendment
    pub bookmark_undo_buffer: Option<(usize, Instant)>,
    /// Pending export overwrite confirmation — set when `/export <name>` is
    /// invoked on a path that already exists. Carries the resolved target
    /// `PathBuf` and the *pre-rendered* markdown content captured at the
    /// moment of confirmation (so the overlay's `y` press writes a stable
    /// snapshot even if the conversation mutates in the background).
    /// Cleared on confirm / cancel / Esc.
    // Covers: Story 4-4 AC12
    pub pending_export: Option<(std::path::PathBuf, String)>,
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
            viewport_height: height.saturating_sub(4),
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
            user_message_boundaries: Vec::new(),
            height_cache: HeightCache::default(),
            pending_anchor: None,
            tool_block_states: HashMap::new(),
            focused_tool_id: None,
            pending_permission: None,
            permission_queue: PermissionQueue::default(),
            pending_feedback_input: None,
            retry_state: None,
            feedback_blocks: BTreeMap::new(),
            active_feedback_id: None,
            ask_user_question: None,
            question_response_tx: None,
            has_project_context: false,
            input_history: InputHistory::new(),
            multiline_mode: false,
            reverse_search: ReverseSearchState::new(),
            search_state: SearchState::new(),
            cross_search: CrossSearchState::new(),
            input_scroll_offset: 0,
            autocomplete: AutocompleteState::new(),
            resolved_mentions: Vec::new(),
            pending_images: Vec::new(),
            pending_large_image: None,
            image_indicator: None,
            command_palette: CommandPaletteState::new(),
            which_key: WhichKeyState::new(),
            help_overlay: HelpOverlayState::new(),
            multiplexer_detected: false,
            is_vscode: false,
            session_count: 0,
            current_hint: None,
            sidebar_visible: false,
            sidebar_panel: None,
            sidebar_selected: 0,
            sidebar_entry_count: 0,
            sidebar_scroll_offset: 0,
            pending_delete: None,
            pending_fork_index: None,
            pending_rewind_index: None,
            rewind_preview: None,
            bookmark_list_selected: 0,
            bookmark_list_count: 0,
            bookmark_undo_buffer: None,
            pending_export: None,
        }
    }
}
