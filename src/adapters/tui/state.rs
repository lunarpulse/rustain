use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::adapters::agent_registry::AgentRegistry;
use crate::adapters::skill_registry::SkillRegistry;
use crate::adapters::tui::refresh_tracker::RefreshTracker;
use crate::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
use crate::adapters::tui::widgets::tool_block::ToolBlockState;
use crate::domain::models::ImageAttachment;
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};
use crate::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use crate::domain::models::plan::{PlanDeviationKind, PlanTaskStatus};
use crate::domain::models::tab::TabId;
use crate::domain::models::tool_call::{ApprovalSource, RequestId};
use crate::domain::models::turn::TurnId;
use crate::domain::models::view_state::SummaryTier;
use crate::domain::models::{
    FeedbackBlock, FocusState, RetryState, StatusState, ToolRisk, UsageInfo,
};
use crate::domain::services::cross_search::CrossSearchResult;
use crate::domain::services::search::SearchMatch;

use super::color_detect::ColorCapability;
use super::theme::Theme;

/// Pending permission request awaiting user response.
pub struct PendingPermission {
    pub id: RequestId,
    pub source: ApprovalSource,
    pub tool_name: String,
    pub tool_input: String,
    pub risk: ToolRisk,
}

/// State for the permission feedback mini-input (AC5).
pub struct FeedbackInputState {
    pub buffer: String,
    pub cursor: usize,
    pub pending_permission: PendingPermission,
}

/// Pending plan approval awaiting user y/a/n/e response.
pub struct PendingPlanApproval {
    pub conversation_id: String,
    pub plan_path: std::path::PathBuf,
    pub contents: String,
    pub summary: String,
}

/// Pending inline PlanCard awaiting user [y]/[e]/[n] response (Story 6-1a).
/// Distinct from PendingPlanApproval (6-0d overlay). At most one per conversation.
#[derive(Debug, Clone)]
pub struct PendingPlanCard {
    pub conversation_id: String,
    pub plan_id: String,
    pub plan_snapshot: crate::domain::models::Plan,
}

/// Story 10.5: pending delegation suggestion card awaiting user d/l response.
pub struct PendingDelegationCard {
    pub conversation_id: crate::domain::models::tab::ConversationId,
    pub plan_id: String,
    pub task_number: u32,
    pub suggestion: crate::domain::services::delegation_decider::DelegationSuggestion,
}

/// Story 11.2a: pending memory-consolidation review card awaiting user y/n.
/// Reuses the plan-card / delegation-card grammar — a list of proposed durable
/// facts, each paired with its selection flag (`true` = will be promoted on
/// confirm). Nothing is written until the user resolves the card.
#[derive(Debug, Clone)]
pub struct PendingConsolidationCard {
    pub conversation_id: crate::domain::models::tab::ConversationId,
    pub proposals: Vec<(crate::domain::models::MemoryFact, bool)>,
}

/// Pending skill trust prompt awaiting user y/n/i response (Story 5-2 AC4).
pub struct SkillTrustState {
    pub skill_name: String,
    pub skill_file: std::path::PathBuf,
    pub response_tx: tokio::sync::oneshot::Sender<crate::domain::models::SkillTrustResponse>,
    /// Cached file content for inspect mode. Populated asynchronously via
    /// `spawn_blocking` on the first `i` press so the render path never blocks
    /// on disk I/O (Story 5-2 H5).
    pub inspect_content: Option<String>,
}

/// Pending user-driven skill activation awaiting a trust response.
///
/// Carries enough context to complete activation + start_turn after the
/// user presses y/n. Replaces the oneshot-channel rendezvous from Story
/// 5-2 initial (which deadlocked inside `tokio::select!`).
///
/// INVARIANT: Populated only by the user-driven slash-command path
/// (`AppEvent::AskActivateSkill`). The model-driven path
/// (`AppEvent::SkillTrustPrompt`) uses `SkillTrustState` above.
#[derive(Debug, Clone)]
pub struct PendingSkillActivation {
    pub skill_name: String,
    pub skill_file: std::path::PathBuf,
    pub arguments: String,
    pub conversation_id: crate::domain::models::tab::ConversationId,
}

/// Queue for permission requests that arrive while another is being displayed.
#[derive(Default)]
pub struct PermissionQueue {
    pub(crate) queue: VecDeque<PendingPermission>,
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

    // NOTE: drain_matching removed in 6-0c — the ApprovalRuntime fast-path
    // handles batch sweep automatically after session-always resolve.
}

/// Direction for boundary navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
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

/// Five-axis key for turn height cache entries.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct HeightKey {
    pub turn_id: TurnId,
    pub expansion: bool,
    pub summary_tier: SummaryTier,
    pub terminal_width: u16,
    pub tool_block_states_version: u64,
}

/// Key for user/system message height cache entries.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct MessageHeightKey {
    pub msg_id: String,
    pub terminal_width: u16,
    pub content_hash: u64,
}

/// Cached layout for a turn, including height and per-part block offsets.
#[derive(Clone, Debug)]
pub struct CachedTurnLayout {
    pub height: usize,
    pub block_offsets: Vec<usize>,
}

/// Cache of rendered line heights, keyed by turn or message.
///
/// # Invalidation triggers
/// - `invalidate_all()`: resize, tier toggle
/// - `invalidate_turn(turn_id)`: fold toggle on a specific turn
/// - `evict_turns_not_in(live)`: rewind / fork / compact (turn list shrinks)
///
/// # Key axes
/// Turn entries use `HeightKey` (5-axis: turn_id, expansion, summary_tier,
/// terminal_width, tool_block_states_version). Message entries use
/// `MessageHeightKey` (3-axis: msg_id, terminal_width, content_hash).
#[derive(Debug, Default)]
pub struct HeightCache {
    pub entries: HashMap<HeightKey, CachedTurnLayout>,
    pub message_entries: HashMap<MessageHeightKey, usize>,
    pub last_seen_turn_count: usize,
}

impl HeightCache {
    /// Get cached layout for a turn key.
    pub fn get(&self, key: &HeightKey) -> Option<&CachedTurnLayout> {
        self.entries.get(key)
    }

    /// Get cached height for a message key.
    pub fn get_message(&self, key: &MessageHeightKey) -> Option<usize> {
        self.message_entries.get(key).copied()
    }

    /// Set cached layout for a turn key.
    pub fn set(&mut self, key: HeightKey, layout: CachedTurnLayout) {
        self.entries.insert(key, layout);
    }

    /// Set cached height for a message key.
    pub fn set_message(&mut self, key: MessageHeightKey, height: usize) {
        self.message_entries.insert(key, height);
    }

    /// Full invalidation (resize, tier toggle, etc.).
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
        self.message_entries.clear();
        self.last_seen_turn_count = 0;
    }

    /// Surgical invalidation: remove all entries for a specific turn.
    pub fn invalidate_turn(&mut self, turn_id: &TurnId) {
        self.entries.retain(|k, _| &k.turn_id != turn_id);
    }

    /// Evict entries for turns not in the live set.
    pub fn evict_turns_not_in<'a>(&mut self, live: impl Iterator<Item = &'a TurnId>) {
        let live_set: std::collections::HashSet<&TurnId> = live.collect();
        self.entries.retain(|k, _| live_set.contains(&k.turn_id));
    }
}

/// Per-tab render state holding the height cache and related render metadata.
/// Lives in the adapter layer (TuiState side-table) to respect hexagonal
/// architecture — TabState in domain/ cannot hold adapter types.
#[derive(Debug, Default)]
pub struct TabRenderState {
    pub height_cache: HeightCache,
    pub cached_width: Option<u16>,
    pub tool_block_states_version: u64,
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
            Direction::Left | Direction::Right => {}
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

/// Story 6-3: state for the task panel + drill-down detail view.
#[derive(Debug, Clone)]
pub struct TaskPanelState {
    pub selected_index: usize,
    pub last_executed_plan_id: Option<String>,
    pub auto_open_skipped_for_plan: Option<String>,
    pub drill_down_task: Option<u32>,
    /// 6-3 AC7: when `true`, the drill-down `task_detail` view renders the
    /// full result body instead of the half-viewport cap. Toggled by Enter
    /// while inside the drill-down. Reset whenever `drill_down_task` clears.
    pub expanded_detail: bool,
    /// 6-3 AC7: scroll offset (in rendered rows) into the result body within
    /// the drill-down view. j/Down advance, k/Up retreat. The widget clamps
    /// this against `lines.len() - body_height` on each render. Reset on
    /// drill-in, drill-out, and the Enter expand/collapse toggle.
    pub detail_scroll_offset: u16,
    pub task_count: usize,
    /// Conversations where the user explicitly closed the Tasks panel via
    /// `Ctrl+X, T`. Subsequent `PlanExecutionStarted` events suppress auto-open
    /// for these conversations until the user reopens manually OR a plan in
    /// the conversation finishes with `failed > 0` (see PlanCompleted arm).
    /// PD1 (Sally Option 1 + A1).
    pub auto_open_suppressed_conversations: std::collections::HashSet<String>,
    /// Conversations that have already received the one-time hint toast
    /// "Tasks panel hidden for this session — Ctrl+X, T to reopen." Tracked
    /// per-conversation, shown at most once. PD1 (Sally A2).
    pub auto_open_hint_shown_for: std::collections::HashSet<String>,
    /// Story 6.4: task selected for reorder mode.
    pub reorder_mode_for: Option<u32>,
    /// Story 6.4: original task order, restored on Esc.
    pub reorder_original_order: Option<Vec<u32>>,
    /// Story 6.4: (plan_id, deviation_kind) while deviation card is shown.
    pub pending_deviation: Option<(String, PlanDeviationKind)>,
    /// Story 6.4: pending skip cascade confirmation state.
    pub skip_cascade_pending: Option<SkipCascadePending>,
    /// Story 6.4: Some(plan_id) while cancel-plan confirm card is shown.
    pub cancel_plan_confirm: Option<String>,
}

impl Default for TaskPanelState {
    fn default() -> Self {
        Self {
            selected_index: 0,
            last_executed_plan_id: None,
            auto_open_skipped_for_plan: None,
            drill_down_task: None,
            expanded_detail: false,
            detail_scroll_offset: 0,
            task_count: 0,
            auto_open_suppressed_conversations: std::collections::HashSet::new(),
            auto_open_hint_shown_for: std::collections::HashSet::new(),
            reorder_mode_for: None,
            reorder_original_order: None,
            pending_deviation: None,
            skip_cascade_pending: None,
            cancel_plan_confirm: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentPanelState {
    pub selected_index: usize,
    pub cached_entries: Vec<crate::domain::models::subagent_view::AgentRowView>,
    pub drill_down_agent: Option<crate::domain::models::AgentId>,
    pub inspector_scroll_offset: u16,
    pub pending_kill_confirm: Option<crate::domain::models::AgentId>,
    pub spool_tail_cache: std::collections::HashMap<crate::domain::models::AgentId, String>,
}

/// Story 6.4: pending skip cascade confirmation state (AC2).
#[derive(Debug, Clone)]
pub struct SkipCascadePending {
    pub plan_id: String,
    pub source_task: u32,
    pub source_prior_status: PlanTaskStatus,
    pub source_prior_error: Option<String>,
    pub downstream: Vec<u32>,
}

/// Story 6.4: user's choice on the skip cascade card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipCascadeChoice {
    CascadeSkip,
    ContinueAnyway,
    CancelSkip,
}

/// Story 7.4: context warning escalation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWarnLevel {
    None,
    Warn,
    Crit,
}

/// Action dispatched by a which-key chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordAction {
    /// Open a panel.
    OpenPanel(crate::domain::models::visual::PanelType),
    /// Show help overlay.
    #[allow(dead_code)]
    ShowHelp,
    /// Open the model/provider selector overlay (Story 7.2 AC1).
    OpenModelSelector,
    /// Open the profile switcher overlay (Story 8.4 AC-1).
    OpenProfileSwitcher,
    /// Open the usage/cost panel overlay (Story 7.5 AC3).
    OpenUsagePanel,
    /// Set the information density mode (Story 8.4b AC-1).
    SetDensityMode(crate::domain::models::visual::DensityMode),
    /// Not yet implemented — show feedback.
    Noop(String),
}

/// Queued notification for Focus-mode buffering (Story 8.4b AC-8).
/// Lives in adapters/tui/ — a render-buffer concern, not a domain concept.
#[derive(Debug, Clone)]
pub enum QueuedNotification {
    StatusFlash {
        level: crate::domain::models::NoticeLevel,
        message: String,
        duration_ms: u64,
    },
    FeedbackBlock {
        id: String,
        level: crate::domain::models::FeedbackLevel,
        message: String,
    },
}

/// A provider column in the model selector dialog (Story 7.2 AC2).
/// TUI render state — lives in adapters/tui/, NOT domain/.
#[derive(Debug, Clone)]
pub struct ProviderColumn {
    pub provider_id: String,
    pub display_name: String,
    pub healthy: bool,
    pub models: Vec<crate::domain::models::provider::ModelDescriptor>,
}

/// Pending context-window warning state (Story 7.2 AC5).
#[derive(Debug, Clone)]
pub struct ContextWarning {
    pub provider_id: String,
    pub model_id: String,
    pub model_display_name: String,
    pub context_window: u32,
    pub current_tokens: u32,
}

/// State for the model/provider selector overlay (Ctrl+X, M) (Story 7.2 AC1).
/// Modelled on `CommandPaletteState` (uses previous_focus + open/dismiss).
pub struct ModelSelectorState {
    pub active: bool,
    pub columns: Vec<ProviderColumn>,
    pub selected_provider: usize,
    pub selected_model: usize,
    pub previous_focus: Option<FocusState>,
    pub connecting: Option<String>,
    pub pending_context_warning: Option<ContextWarning>,
    pub refreshing: Arc<RefreshTracker>,
    pub search_active: bool,
    pub search_query: String,
    pub filtered_indices: Vec<usize>,
}

impl ModelSelectorState {
    pub fn new() -> Self {
        Self {
            active: false,
            columns: Vec::new(),
            selected_provider: 0,
            selected_model: 0,
            previous_focus: None,
            connecting: None,
            pending_context_warning: None,
            refreshing: RefreshTracker::new(),
            search_active: false,
            search_query: String::new(),
            filtered_indices: Vec::new(),
        }
    }

    /// Open the model selector, seeding selection to the currently-active provider+model.
    pub fn open(
        &mut self,
        current_focus: FocusState,
        columns: Vec<ProviderColumn>,
        active_provider_id: &str,
        active_model_id: &str,
    ) {
        self.active = true;
        self.previous_focus = Some(current_focus);
        self.connecting = None;
        self.pending_context_warning = None;
        self.search_active = false;
        self.search_query.clear();
        self.filtered_indices.clear();
        self.columns = columns;

        self.selected_provider = self
            .columns
            .iter()
            .position(|c| c.provider_id == active_provider_id)
            .unwrap_or(0);

        self.selected_model = self
            .columns
            .get(self.selected_provider)
            .and_then(|col| {
                col.models
                    .iter()
                    .position(|m| m.model_id == active_model_id)
            })
            .unwrap_or(0);
    }

    /// Dismiss the selector, returning the previous focus to restore.
    pub fn dismiss(&mut self) -> Option<FocusState> {
        self.active = false;
        self.columns.clear();
        self.connecting = None;
        self.pending_context_warning = None;
        self.search_active = false;
        self.search_query.clear();
        self.filtered_indices.clear();
        self.previous_focus.take()
    }

    /// Navigate between providers (Left/Right). Wraps around; no-op when ≤1 column (AC7).
    pub fn navigate_provider(&mut self, direction: Direction) {
        if self.columns.len() <= 1 {
            return;
        }
        match direction {
            Direction::Left => {
                if self.selected_provider == 0 {
                    self.selected_provider = self.columns.len() - 1;
                } else {
                    self.selected_provider -= 1;
                }
            }
            Direction::Right => {
                self.selected_provider = (self.selected_provider + 1) % self.columns.len();
            }
            _ => {}
        }
        self.selected_model = 0;
        self.search_active = false;
        self.search_query.clear();
        self.filtered_indices.clear();
    }

    /// Navigate between models (Up/Down). Wraps within current column.
    pub fn navigate_model(&mut self, direction: Direction) {
        let count = self
            .columns
            .get(self.selected_provider)
            .map(|c| c.models.len())
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        match direction {
            Direction::Up => {
                if self.selected_model == 0 {
                    self.selected_model = count - 1;
                } else {
                    self.selected_model -= 1;
                }
            }
            Direction::Down => {
                self.selected_model = (self.selected_model + 1) % count;
            }
            _ => {}
        }
    }

    /// Recompute filtered_indices based on search_query.
    pub fn recompute_filter(&mut self) {
        self.filtered_indices.clear();
        if !self.search_active || self.search_query.is_empty() {
            return;
        }
        let needle = self.search_query.to_lowercase();
        if let Some(col) = self.columns.get(self.selected_provider) {
            for (i, m) in col.models.iter().enumerate() {
                let id_lc = m.model_id.to_lowercase();
                let dn_lc = m.display_name.to_lowercase();
                if id_lc.contains(&needle) || dn_lc.contains(&needle) {
                    self.filtered_indices.push(i);
                }
            }
        }
        self.selected_model = 0;
        // Defensive clamp invariant (Preflight Consensus #6)
        if !self.filtered_indices.is_empty() && self.selected_model >= self.filtered_indices.len() {
            self.selected_model = self.filtered_indices.len() - 1;
        }
    }

    /// Return the effective index into columns[selected_provider].models.
    pub fn selected_model_index(&self) -> Option<usize> {
        if self.search_active && !self.search_query.is_empty() {
            self.filtered_indices.get(self.selected_model).copied()
        } else {
            let col = self.columns.get(self.selected_provider)?;
            if self.selected_model < col.models.len() {
                Some(self.selected_model)
            } else {
                None
            }
        }
    }

    /// Get the currently selected (provider_id, model).
    pub fn selected(&self) -> Option<(&str, &crate::domain::models::provider::ModelDescriptor)> {
        let col = self.columns.get(self.selected_provider)?;
        let idx = self.selected_model_index()?;
        let model = col.models.get(idx)?;
        Some((&col.provider_id, model))
    }

    /// Clone the RefreshTracker handle for spawning background tasks.
    pub fn refreshing_handle(&self) -> Arc<RefreshTracker> {
        Arc::clone(&self.refreshing)
    }
}

impl Default for ModelSelectorState {
    fn default() -> Self {
        Self::new()
    }
}

// ── ProfileSwitcher (Story 8.4 AC-1) ─────────────────────────────────────

/// State for the profile switcher overlay (Ctrl+X, P).
/// Modelled on `ModelSelectorState` (uses previous_focus + open/dismiss).
#[derive(Debug, Clone)]
pub struct ProfileSwitcherState {
    pub active: bool,
    pub profiles: Vec<crate::domain::models::ProfileDescriptor>,
    pub selected: usize,
    pub active_index: Option<usize>,
    pub preview: Option<crate::domain::services::swap_tier::TransitionPlan>,
    pub previous_focus: Option<FocusState>,
    pub current_dimensions: std::collections::BTreeMap<
        crate::domain::models::PortDimension,
        crate::domain::models::AdapterRef,
    >,
}

impl ProfileSwitcherState {
    pub fn new() -> Self {
        Self {
            active: false,
            profiles: Vec::new(),
            selected: 0,
            active_index: None,
            preview: None,
            previous_focus: None,
            current_dimensions: std::collections::BTreeMap::new(),
        }
    }

    pub fn open(
        &mut self,
        current_focus: FocusState,
        profiles: Vec<crate::domain::models::ProfileDescriptor>,
        active_name: &str,
        current_dims: std::collections::BTreeMap<
            crate::domain::models::PortDimension,
            crate::domain::models::AdapterRef,
        >,
    ) {
        self.active = true;
        self.profiles = profiles;
        self.active_index = self.profiles.iter().position(|p| p.name == active_name);
        self.selected = self.active_index.unwrap_or(0);
        self.previous_focus = Some(current_focus);
        self.current_dimensions = current_dims;
        self.preview = None;
    }

    pub fn dismiss(&mut self) -> Option<FocusState> {
        self.active = false;
        self.preview = None;
        self.previous_focus.take()
    }

    pub fn navigate(&mut self, direction: i32) {
        if self.profiles.is_empty() {
            return;
        }
        let len = self.profiles.len();
        self.selected = ((self.selected as i32 + direction).rem_euclid(len as i32)) as usize;
    }

    pub fn selected_profile(&self) -> Option<&crate::domain::models::ProfileDescriptor> {
        self.profiles.get(self.selected)
    }

    pub fn enter_preview(&mut self, plan: crate::domain::services::swap_tier::TransitionPlan) {
        self.preview = Some(plan);
    }

    pub fn exit_preview(&mut self) {
        self.preview = None;
    }

    pub fn compute_diff_for_selected(
        &self,
        profile: &crate::domain::models::ProfileDescriptor,
    ) -> Option<crate::domain::services::swap_tier::TransitionPlan> {
        if self.current_dimensions.is_empty() {
            return None;
        }
        let plan = crate::domain::services::swap_tier::TransitionPlan::from_selections(
            &self.current_dimensions,
            &profile.selection.dimensions,
            &profile.name,
            profile.identity_color.0,
            None,
        );
        Some(plan)
    }
}

impl Default for ProfileSwitcherState {
    fn default() -> Self {
        Self::new()
    }
}

// ── UsagePanel (Story 7.5 AC3) ────────────────────────────────────────────

/// One row in the panel's "Turn breakdown" section.
#[derive(Debug, Clone)]
pub struct TurnUsageRow {
    pub turn_index: u32,
    pub model: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    /// `None` when no pricing entry exists for the model (AC6 → "n/a").
    pub cost_usd: Option<f64>,
}

/// Today's session aggregate (Story 7.5 AC3).
#[derive(Debug, Clone, Default)]
pub struct SessionUsageSummary {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: Option<f64>,
    pub task_count: u32,
    pub elapsed_secs: i64,
    pub cache_read_tokens: u64,
    pub cache_total_tokens: u64,
    pub cache_savings_usd: f64,
}

/// Daily budget warning state (Story 7.5 AC5).
#[derive(Debug, Clone, Default)]
pub struct DailyBudgetState {
    pub spent_today_usd: f64,
    pub limit_usd: f64,
    pub computed_at_ms: i64,
    pub dismissed_until_unix: i64,
}

impl DailyBudgetState {
    pub fn percent(&self) -> u32 {
        if self.limit_usd <= 0.0 {
            return 0;
        }
        ((self.spent_today_usd / self.limit_usd) * 100.0)
            .round()
            .min(u32::MAX as f64) as u32
    }
}

/// State for the usage/cost panel overlay (Ctrl+X, U). Story 7.5 AC3 / UX-DR111.
/// Modelled on `ModelSelectorState` (open/dismiss + previous_focus).
pub struct UsagePanelState {
    pub active: bool,
    pub previous_focus: Option<FocusState>,
    /// Logical section cursor (0..4) — visual highlight only.
    pub selected_section: usize,
    pub turn_rows: Vec<TurnUsageRow>,
    pub session_today: SessionUsageSummary,
    pub per_model:
        std::collections::BTreeMap<String, crate::domain::services::cost_calculator::ModelCost>,
    pub missing_pricing_models: Vec<String>,
    /// Context window snapshot at panel-open (in/out + window). Read from
    /// `state.token_usage` / active model at open-time so the panel doesn't
    /// poll providers.
    pub context_used_tokens: u32,
    pub context_window_tokens: u32,
}

impl UsagePanelState {
    pub fn new() -> Self {
        Self {
            active: false,
            previous_focus: None,
            selected_section: 0,
            turn_rows: Vec::new(),
            session_today: SessionUsageSummary::default(),
            per_model: std::collections::BTreeMap::new(),
            missing_pricing_models: Vec::new(),
            context_used_tokens: 0,
            context_window_tokens: 0,
        }
    }

    /// Open the panel and stash the previous focus for Esc-dismiss.
    pub fn open(&mut self, current_focus: FocusState) {
        self.active = true;
        self.previous_focus = Some(current_focus);
        self.selected_section = 0;
    }

    /// Dismiss the panel, returning the focus to restore.
    pub fn dismiss(&mut self) -> Option<FocusState> {
        self.active = false;
        self.previous_focus.take()
    }
}

impl Default for UsagePanelState {
    fn default() -> Self {
        Self::new()
    }
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
            Direction::Left | Direction::Right => {}
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
        chord_map.insert('p', ChordAction::OpenProfileSwitcher);
        chord_map.insert('m', ChordAction::OpenModelSelector);
        chord_map.insert(
            'a',
            ChordAction::OpenPanel(crate::domain::models::visual::PanelType::Adapters),
        );
        chord_map.insert(
            's',
            ChordAction::OpenPanel(crate::domain::models::visual::PanelType::Agents),
        );
        chord_map.insert('l', ChordAction::Noop("Log panel — Epic 14".to_string()));
        chord_map.insert(
            't',
            ChordAction::OpenPanel(crate::domain::models::visual::PanelType::Tasks),
        );
        chord_map.insert('u', ChordAction::OpenUsagePanel);
        chord_map.insert(
            'w',
            ChordAction::SetDensityMode(crate::domain::models::visual::DensityMode::Monitor),
        );
        chord_map.insert(
            'f',
            ChordAction::SetDensityMode(crate::domain::models::visual::DensityMode::Focus),
        );
        chord_map.insert(
            'd',
            ChordAction::SetDensityMode(crate::domain::models::visual::DensityMode::Dashboard),
        );
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
    pub active_tab_id: TabId,
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
    /// Scroll position snapshot synced from `view_state.scroll_offset` before
    /// each render/input cycle. Read by app.rs boundary handlers and event_loop.
    /// NEVER write directly — use `dispatch_view_scroll` or `reconcile_fold_toggle`.
    pub(crate) scroll_snapshot: usize,
    /// Auto-scroll snapshot synced from `view_state.mode == Following`.
    /// Marked pub(crate) to prevent external crate access (tests migrate to view_state reads).
    pub(crate) auto_snapshot: bool,
    pub total_content_height: usize,
    /// Line offsets for each content block boundary in rendered view.
    pub block_boundaries: Vec<usize>,
    /// Line offsets for each message boundary (all roles) in rendered view.
    /// Drives the status-bar position counter and rewind/fork targeting.
    pub message_boundaries: Vec<usize>,
    /// Line offsets for each **user** message boundary in rendered view.
    /// Drives `{`/`}` jump-between-turn navigation.
    pub user_message_boundaries: Vec<usize>,
    /// Per-tab render states for height cache and related render metadata.
    /// Side-table pattern respects hexagonal architecture (TabState in domain/
    /// cannot hold adapter-side HeightCache).
    pub tab_render_states: HashMap<TabId, TabRenderState>,
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
    /// Whether a Ctrl+K chord leader has been pressed and the next character key
    /// should be dispatched through FeedbackAction::dispatch_key().
    pub chord_leader_active: bool,
    /// Story 16.6: pending vim `z`-prefix chord (za/zc/zo/zM/zR/zs/zz). AC10
    pub pending_z: bool,
    /// Story 16.6: pending vim `]`/`[` bracket chord (]]/[[/]P).
    /// Story 16.8 preflight rebinding (2026-05-03) added `]P` as the home for
    /// `JumpToLatestProseAnchor` (relocated from S16.6's `G` binding per ADR-16-03).
    /// AC10
    pub pending_bracket: Option<char>,
    /// Story 16.8: pending `g`-prefix chord for `gg` detection. AC2
    /// Single-`g` immediately fires ScrollToTop (legacy alias preserved) AND
    /// sets this flag; a follow-up `g` produces the idempotent `gg` chord.
    /// Reset on any non-`g` key or non-key event via the chord-reset guard.
    pub pending_g: bool,
    /// S16.8 AC15: Two-stage anchor-confirmation gate.  When the user is Pinned
    /// and emits a scroll-intent event, this field records the first tick's
    /// instant.  A second scroll-intent within 2000ms drops the anchor via
    /// `ViewEvent::DropAnchorAndScroll`.  Reset on `]]`/`[[` or mode change.
    pub pending_anchor_drop: Option<std::time::Instant>,
    /// Active AskUserQuestion card state.
    pub ask_user_question: Option<AskUserQuestionState>,
    /// Oneshot sender for AskUserQuestion responses.
    pub question_response_tx: Option<tokio::sync::oneshot::Sender<String>>,
    /// Pending skill trust prompt state (Story 5-2 AC4).
    pub pending_skill_trust: Option<SkillTrustState>,
    /// Queue for additional skill trust prompts that arrive while one is displayed.
    pub skill_trust_queue: VecDeque<SkillTrustState>,
    /// Whether the skill trust prompt is in inspection mode.
    pub skill_trust_inspect_mode: bool,
    /// User-driven slash-command activation awaiting trust confirmation.
    /// Parallel to `pending_skill_trust` (model-driven). Populated by
    /// `AskActivateSkill` handler, consumed by `SkillTrustAccept`/`Decline`.
    pub pending_activation: Option<PendingSkillActivation>,
    /// Cached inspect-mode content for the user-driven pending activation.
    /// Populated asynchronously via `spawn_blocking` on the first `i` press.
    /// Cleared when `pending_activation` is consumed.
    pub pending_activation_inspect_content: Option<String>,
    /// Whether project context files were loaded (for status bar indicator).
    pub has_project_context: bool,
    /// Session-scoped input history for Up/Down and Ctrl+R.
    // Covers: UX-DR74
    pub input_history: InputHistory,
    /// Whether multi-line mode is active (toggled by Ctrl+E).
    // Covers: UX-DR76
    pub multiline_mode: bool,
    /// Story 11.4 (AC7) — whether memory-based context injection is enabled for
    /// this session. Toggled by `/context off` / `/context on`. Default `true`.
    /// Session-scoped: persists across `/new` (like `selected_model`, Q3). When
    /// `false`, the turn seam fully short-circuits — zero `MemoryPort` calls.
    /// Project context (CLAUDE.md) is unaffected (it is structural, persona-owned).
    pub context_injection_on: bool,
    /// Story 11.4 (Task 4.4) — the LAST assembled `ContextBundle`, cached so
    /// `/context show` and the status-bar token cost read it without re-running
    /// `assemble()`. `None` until the first injected turn.
    pub last_context_bundle: Option<crate::domain::models::ContextBundle>,
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
    /// Model/provider selector state (Ctrl+X, M) (Story 7.2).
    pub model_selector: ModelSelectorState,
    /// Profile switcher state (Ctrl+X, P) (Story 8.4 AC-1).
    pub profile_switcher: ProfileSwitcherState,
    /// Active-profile snapshot — identity color + name for status bar.
    /// Initialized at startup; updated on ProfileSwitched events.
    pub active_profile: Option<crate::domain::models::ActiveProfileSnapshot>,
    /// Session-scoped adapter overrides (Story 8.5 AC-5). BTreeMap for
    /// deterministic iteration in panel + tests. Single-writer rule: only
    /// the override handler and startup hook mutate this.
    pub session_overrides: std::collections::BTreeMap<
        crate::domain::models::PortDimension,
        crate::domain::models::AdapterRef,
    >,
    /// Usage/cost panel state (Ctrl+X, U) (Story 7.5 AC3).
    pub usage_panel: UsagePanelState,
    /// Daily budget warning state (Story 7.5 AC5). `None` when budget disabled.
    pub daily_budget: Option<DailyBudgetState>,
    /// Captured resolved-model from `start_turn_inner`, consumed-and-cleared on
    /// the first subsequent `ChunkAction::TurnComplete` to stamp `Turn.model`
    /// (Story 7.5 AC1.5).
    pub pending_resolved_model: Option<String>,
    /// Session-scoped active-model override (Story 7.2 AC3).
    /// `None` = use `config.model`. Runtime hot-swap — NOT a config mutation.
    pub selected_model: Option<String>,
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
    /// Current information density mode (Story 8.4b).
    /// Controls layout density, sidebar visibility policy, and notification routing.
    pub density_mode: crate::domain::models::visual::DensityMode,
    /// Epic-10 hook: auto-trigger logic (e.g., InteractionState::Executing → Monitor)
    /// MUST check this flag and skip the auto-switch when true. Story 8.4b establishes
    /// the contract; Story 10.x (multi-agent execution UX) implements the auto-trigger.
    pub density_user_overridden: bool,
    /// Notification queue for Focus-mode buffering (Story 8.4b AC-8).
    /// Bounded at 32; oldest dropped on overflow. Drains FIFO on transition out of Focus.
    pub queued_notifications: VecDeque<QueuedNotification>,
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
    /// Story 6-3: state for the task panel sidebar and drill-down detail view.
    pub task_panel_state: TaskPanelState,
    pub agent_panel_state: AgentPanelState,
    /// Story 6-3 (PD4): resolved value of `[layout.auto_panels] on_task_plan`.
    /// `"tasks"` (default) or `"none"`. Read by the `PlanExecutionStarted`
    /// event arm to decide whether to auto-open the Tasks panel.
    pub auto_open_on_task_plan: String,
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
    /// Skill registry — populated by background discovery task (Story 5-1 AC6).
    /// Shared via Arc<tokio::sync::RwLock> between TuiState and SkillActivator
    /// so autocomplete and tool-dispatch see the same catalog (DF-159, Story 16-0 AC1).
    pub skill_registry: Arc<tokio::sync::RwLock<SkillRegistry>>,
    /// Synchronous cache of skill names for the hot input path (Story 16-0).
    /// Refreshed from the shared registry whenever skills are discovered.
    pub skill_name_cache: HashSet<String>,
    pub active_skill_count: usize,
    pub agent_registry: AgentRegistry,
    pub agent_suggestions: Vec<AutocompleteSuggestion>,
    pub active_agent_name: Option<String>,
    /// Pending agent activation queued while background discovery is in-flight.
    /// Set by `submit_message` when registry is not yet discovered;
    /// consumed by `AgentsDiscovered` handler in the event loop.
    pub pending_agent_activation: Option<(String, Option<String>)>,
    /// Pending plan approval card state (Story 6-0d AC4).
    pub pending_plan_approval: Option<PendingPlanApproval>,
    /// Pending inline PlanCard awaiting user resolution (Story 6-1a).
    pub pending_plan_card: Option<PendingPlanCard>,
    /// Story 10.5: pending delegation suggestion card awaiting user d/l response.
    pub pending_delegation_card: Option<PendingDelegationCard>,
    /// Story 11.2a: pending memory-consolidation review card awaiting user y/n.
    pub pending_consolidation_card: Option<PendingConsolidationCard>,
    /// Story 6-2a: pending AgentThenSubmit (synthetic task turn) queued
    /// when the event arrives while a stream is still active. Dispatched
    /// after the stream completes (TurnComplete handler).
    pub pending_agent_then_submit: Option<(String, bool)>,
    /// Which assistant-turn count the last plan reminder was injected at.
    /// `None` when no reminder is pending.
    pub pending_plan_reminder_at_turn: Option<u32>,
    /// Active plan file path when in Plan mode. `None` otherwise.
    pub plan_file_path: Option<std::path::PathBuf>,
    /// Story 7.4: whether a compaction operation is in progress.
    pub compacting: bool,
    /// Story 7.4: current context warning level.
    pub context_warn_level: ContextWarnLevel,
    /// Story 7.4: pending context carryover for fresh tab + summary injection.
    pub pending_context_carryover: Option<String>,
}

impl TuiState {
    #[allow(dead_code)]
    pub fn new(width: u16, height: u16) -> Self {
        Self::with_capability(width, height, ColorCapability::TrueColor)
    }

    pub fn with_capability(width: u16, height: u16, capability: ColorCapability) -> Self {
        Self {
            active_tab_id: 0,
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
            auto_snapshot: true,
            scroll_snapshot: 0,
            total_content_height: 0,
            block_boundaries: Vec::new(),
            message_boundaries: Vec::new(),
            user_message_boundaries: Vec::new(),
            tab_render_states: HashMap::new(),
            pending_anchor: None,
            tool_block_states: HashMap::new(),
            focused_tool_id: None,
            pending_permission: None,
            permission_queue: PermissionQueue::default(),
            pending_feedback_input: None,
            retry_state: None,
            feedback_blocks: BTreeMap::new(),
            active_feedback_id: None,
            chord_leader_active: false,
            pending_z: false,
            pending_bracket: None,
            pending_g: false,
            pending_anchor_drop: None,
            ask_user_question: None,
            question_response_tx: None,
            pending_skill_trust: None,
            skill_trust_queue: VecDeque::new(),
            skill_trust_inspect_mode: false,
            pending_activation: None,
            pending_activation_inspect_content: None,
            has_project_context: false,
            input_history: InputHistory::new(),
            multiline_mode: false,
            context_injection_on: true,
            last_context_bundle: None,
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
            model_selector: ModelSelectorState::new(),
            profile_switcher: ProfileSwitcherState::new(),
            active_profile: None,
            usage_panel: UsagePanelState::new(),
            daily_budget: None,
            pending_resolved_model: None,
            selected_model: None,
            multiplexer_detected: false,
            is_vscode: false,
            session_count: 0,
            current_hint: None,
            density_mode: Default::default(),
            session_overrides: std::collections::BTreeMap::new(),
            density_user_overridden: false,
            queued_notifications: VecDeque::new(),
            sidebar_visible: false,
            sidebar_panel: None,
            sidebar_selected: 0,
            sidebar_entry_count: 0,
            sidebar_scroll_offset: 0,
            task_panel_state: TaskPanelState::default(),
            agent_panel_state: AgentPanelState::default(),
            auto_open_on_task_plan: "tasks".to_string(),
            pending_delete: None,
            pending_fork_index: None,
            pending_rewind_index: None,
            rewind_preview: None,
            bookmark_list_selected: 0,
            bookmark_list_count: 0,
            bookmark_undo_buffer: None,
            pending_export: None,
            skill_registry: Arc::new(tokio::sync::RwLock::new(SkillRegistry::new())),
            skill_name_cache: HashSet::new(),
            active_skill_count: 0,
            agent_registry: AgentRegistry::new(),
            agent_suggestions: Vec::new(),
            active_agent_name: None,
            pending_agent_activation: None,
            pending_plan_approval: None,
            pending_plan_card: None,
            pending_delegation_card: None,
            pending_consolidation_card: None,
            pending_agent_then_submit: None,
            pending_plan_reminder_at_turn: None,
            plan_file_path: None,
            compacting: false,
            context_warn_level: ContextWarnLevel::None,
            pending_context_carryover: None,
        }
    }

    pub async fn refresh_skill_name_cache(&mut self) {
        let guard = self.skill_registry.read().await;
        self.skill_name_cache = guard.skills().iter().map(|s| s.name.clone()).collect();
    }

    pub fn replace_agent_registry(&mut self, registry: AgentRegistry) {
        self.agent_registry = registry;
    }

    pub fn task_panel_max_index(&self) -> usize {
        self.task_panel_state.task_count.saturating_sub(1)
    }

    pub fn selected_agent(&self) -> Option<&crate::domain::models::subagent_view::AgentRowView> {
        self.agent_panel_state
            .cached_entries
            .get(self.agent_panel_state.selected_index)
    }

    pub fn refresh_agent_suggestions(&mut self) {
        use crate::domain::models::autocomplete::AutocompleteSuggestion;
        let filter = self.autocomplete.filter_text.to_lowercase();
        let agents = self.agent_registry.filter(&filter);
        let has_agents = !agents.is_empty();
        let mut suggestions: Vec<AutocompleteSuggestion> =
            vec![AutocompleteSuggestion::AgentMention {
                name: "default".to_string(),
                description: if has_agents {
                    "Clear active agent — return to project-context persona".to_string()
                } else {
                    "No custom agents discovered — type @Agents/default to clear any active agent"
                        .to_string()
                },
            }];
        for def in agents {
            suggestions.push(AutocompleteSuggestion::AgentMention {
                name: def.name.clone(),
                description: def.description.clone(),
            });
        }
        self.agent_suggestions = suggestions;
    }

    /// Get or create the render state for a specific tab.
    pub fn tab_render_state(&mut self, tab_id: TabId) -> &mut TabRenderState {
        self.tab_render_states.entry(tab_id).or_default()
    }

    /// Public getter for scroll_snapshot (pub(crate) field access for external crates like tests).
    /// Read this value for the current scroll position. Write through dispatch_view_scroll.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_snapshot
    }

    /// Public getter for auto_snapshot. True when view_state.mode == Following.
    pub fn auto_scroll(&self) -> bool {
        self.auto_snapshot
    }

    /// Public setter for scroll_snapshot. For test setup only. Production code
    /// must use `dispatch_view_scroll` which syncs from view_state.
    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_snapshot = offset;
    }

    /// Public setter for auto_snapshot. For test setup only.
    pub fn set_auto_scroll(&mut self, auto: bool) {
        self.auto_snapshot = auto;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pending(name: &str) -> PendingSkillActivation {
        PendingSkillActivation {
            skill_name: name.to_string(),
            skill_file: std::path::PathBuf::from("/test/skills")
                .join(name)
                .join("SKILL.md"),
            arguments: String::new(),
            conversation_id: crate::domain::models::tab::ConversationId::new(),
        }
    }

    #[test]
    fn test_tui_state_pending_activation_initializes_none() {
        let state = TuiState::new(80, 24);
        assert!(state.pending_activation.is_none());
    }

    #[test]
    fn test_pending_activation_stores_and_takes() {
        let mut state = TuiState::new(80, 24);
        let pending = make_pending("reviewer");
        let cid = pending.conversation_id.clone();

        state.pending_activation = Some(pending);
        assert!(state.pending_activation.is_some());
        assert_eq!(
            state.pending_activation.as_ref().unwrap().skill_name,
            "reviewer"
        );
        assert_eq!(
            state.pending_activation.as_ref().unwrap().conversation_id,
            cid
        );

        let taken = state.pending_activation.take();
        assert!(taken.is_some());
        assert!(state.pending_activation.is_none());
    }

    #[test]
    fn test_pending_activation_does_not_affect_skill_trust_state() {
        let mut state = TuiState::new(80, 24);
        let pending = make_pending("reviewer");
        state.pending_activation = Some(pending);
        assert!(state.pending_skill_trust.is_none());
        assert!(state.skill_trust_queue.is_empty());
    }

    #[test]
    fn test_accept_takes_user_driven_first() {
        let mut state = TuiState::new(80, 24);
        let pending_user = make_pending("user-skill");
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let pending_model = SkillTrustState {
            skill_name: "model-skill".to_string(),
            skill_file: std::path::PathBuf::from("/test/model/SKILL.md"),
            response_tx: tx,
            inspect_content: None,
        };
        state.pending_activation = Some(pending_user);
        state.pending_skill_trust = Some(pending_model);

        let taken_user = state.pending_activation.take();
        assert!(taken_user.is_some());
        assert_eq!(taken_user.unwrap().skill_name, "user-skill");
        assert!(state.pending_skill_trust.is_some());
        assert_eq!(
            state.pending_skill_trust.as_ref().unwrap().skill_name,
            "model-skill"
        );
    }

    #[test]
    fn test_decline_clears_focus_when_no_model_pending() {
        let mut state = TuiState::new(80, 24);
        state.pending_activation = Some(make_pending("decline-me"));
        state.focus = crate::domain::models::FocusState::Overlay(
            crate::domain::models::visual::OverlayType::Confirmation(
                crate::domain::models::visual::ConfirmationType::SkillTrust,
            ),
        );

        let _taken = state.pending_activation.take();
        if state.pending_skill_trust.is_none() {
            state.focus = crate::domain::models::FocusState::Input;
            state.skill_trust_inspect_mode = false;
        }
        assert!(state.pending_activation.is_none());
        assert_eq!(state.focus, crate::domain::models::FocusState::Input);
        assert!(!state.skill_trust_inspect_mode);
    }

    #[test]
    fn test_cancel_drains_pending_activation() {
        let mut state = TuiState::new(80, 24);
        state.pending_activation = Some(make_pending("cancel-me"));
        state.focus = crate::domain::models::FocusState::Overlay(
            crate::domain::models::visual::OverlayType::Confirmation(
                crate::domain::models::visual::ConfirmationType::SkillTrust,
            ),
        );

        if let Some(_pending) = state.pending_activation.take() {
            if state.pending_skill_trust.is_none() {
                state.focus = crate::domain::models::FocusState::Input;
                state.skill_trust_inspect_mode = false;
            }
            state.needs_redraw = true;
        }

        assert!(state.pending_activation.is_none());
        assert_eq!(state.focus, crate::domain::models::FocusState::Input);
        assert!(state.needs_redraw);
    }

    #[test]
    fn test_double_pending_activation_dropped() {
        let mut state = TuiState::new(80, 24);
        state.pending_activation = Some(make_pending("first"));
        assert!(state.pending_activation.is_some());

        let second = make_pending("second");
        if state.pending_activation.is_some() {
            // Simulates the error path: second activation is dropped
        } else {
            state.pending_activation = Some(second);
        }
        assert_eq!(
            state.pending_activation.as_ref().unwrap().skill_name,
            "first"
        );
    }

    // ── HeightCache tests (Story 16-5) ──

    #[test]
    fn height_cache_get_set_roundtrip() {
        let mut cache = HeightCache::default();
        let key = HeightKey {
            turn_id: crate::domain::models::TurnId("t1".into()),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 0,
        };
        let layout = CachedTurnLayout {
            height: 42,
            block_offsets: vec![0, 10, 20],
        };
        cache.set(key.clone(), layout.clone());
        assert_eq!(cache.get(&key).unwrap().height, 42);
        assert_eq!(cache.get(&key).unwrap().block_offsets, vec![0, 10, 20]);
    }

    #[test]
    fn height_cache_miss_returns_none() {
        let cache = HeightCache::default();
        let key = HeightKey {
            turn_id: crate::domain::models::TurnId("t1".into()),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 0,
        };
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn height_cache_invalidate_all_clears_entries() {
        let mut cache = HeightCache::default();
        let key = HeightKey {
            turn_id: crate::domain::models::TurnId("t1".into()),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 0,
        };
        cache.set(
            key.clone(),
            CachedTurnLayout {
                height: 5,
                block_offsets: vec![],
            },
        );
        cache.invalidate_all();
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.last_seen_turn_count, 0);
    }

    #[test]
    fn height_cache_invalidate_turn_removes_only_target_turn() {
        let mut cache = HeightCache::default();
        let t1 = crate::domain::models::TurnId("t1".into());
        let t2 = crate::domain::models::TurnId("t2".into());
        cache.set(
            HeightKey {
                turn_id: t1.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
                tool_block_states_version: 0,
            },
            CachedTurnLayout {
                height: 1,
                block_offsets: vec![],
            },
        );
        cache.set(
            HeightKey {
                turn_id: t2.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
                tool_block_states_version: 0,
            },
            CachedTurnLayout {
                height: 2,
                block_offsets: vec![],
            },
        );
        cache.invalidate_turn(&t1);
        assert!(
            cache
                .get(&HeightKey {
                    turn_id: t1.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                    tool_block_states_version: 0
                })
                .is_none()
        );
        assert_eq!(
            cache
                .get(&HeightKey {
                    turn_id: t2.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                    tool_block_states_version: 0
                })
                .unwrap()
                .height,
            2
        );
    }

    #[test]
    fn height_cache_evict_turns_not_in_removes_stale() {
        let mut cache = HeightCache::default();
        let t1 = crate::domain::models::TurnId("t1".into());
        let t2 = crate::domain::models::TurnId("t2".into());
        let t3 = crate::domain::models::TurnId("t3".into());
        cache.set(
            HeightKey {
                turn_id: t1.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
                tool_block_states_version: 0,
            },
            CachedTurnLayout {
                height: 1,
                block_offsets: vec![],
            },
        );
        cache.set(
            HeightKey {
                turn_id: t2.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
                tool_block_states_version: 0,
            },
            CachedTurnLayout {
                height: 2,
                block_offsets: vec![],
            },
        );
        cache.set(
            HeightKey {
                turn_id: t3.clone(),
                expansion: true,
                summary_tier: SummaryTier::Tier1,
                terminal_width: 80,
                tool_block_states_version: 0,
            },
            CachedTurnLayout {
                height: 3,
                block_offsets: vec![],
            },
        );
        cache.evict_turns_not_in([&t1, &t3].into_iter());
        assert!(
            cache
                .get(&HeightKey {
                    turn_id: t1.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                    tool_block_states_version: 0
                })
                .is_some()
        );
        assert!(
            cache
                .get(&HeightKey {
                    turn_id: t2.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                    tool_block_states_version: 0
                })
                .is_none()
        );
        assert!(
            cache
                .get(&HeightKey {
                    turn_id: t3.clone(),
                    expansion: true,
                    summary_tier: SummaryTier::Tier1,
                    terminal_width: 80,
                    tool_block_states_version: 0
                })
                .is_some()
        );
    }

    #[test]
    fn height_cache_message_get_set_roundtrip() {
        let mut cache = HeightCache::default();
        let key = MessageHeightKey {
            msg_id: "m1".into(),
            terminal_width: 80,
            content_hash: 12345,
        };
        cache.set_message(key.clone(), 17);
        assert_eq!(cache.get_message(&key), Some(17));
    }

    #[test]
    fn height_cache_message_miss_returns_none() {
        let cache = HeightCache::default();
        let key = MessageHeightKey {
            msg_id: "m1".into(),
            terminal_width: 80,
            content_hash: 12345,
        };
        assert_eq!(cache.get_message(&key), None);
    }

    #[test]
    fn height_cache_invalidate_all_clears_message_entries() {
        let mut cache = HeightCache::default();
        let key = MessageHeightKey {
            msg_id: "m1".into(),
            terminal_width: 80,
            content_hash: 12345,
        };
        cache.set_message(key.clone(), 17);
        cache.invalidate_all();
        assert_eq!(cache.get_message(&key), None);
    }

    #[test]
    fn test_model_selector_provider_nav_wraps() {
        use crate::domain::models::provider::{ModelCapability, ModelDescriptor};
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![
            ProviderColumn {
                provider_id: "a".to_string(),
                display_name: "A".to_string(),
                healthy: true,
                models: vec![ModelDescriptor {
                    model_id: "m1".to_string(),
                    display_name: "M1".to_string(),
                    provider_id: "a".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                }],
            },
            ProviderColumn {
                provider_id: "b".to_string(),
                display_name: "B".to_string(),
                healthy: true,
                models: vec![ModelDescriptor {
                    model_id: "m2".to_string(),
                    display_name: "M2".to_string(),
                    provider_id: "b".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                }],
            },
        ];
        ms.selected_provider = 0;
        ms.navigate_provider(Direction::Left);
        assert_eq!(ms.selected_provider, 1);
        ms.navigate_provider(Direction::Right);
        assert_eq!(ms.selected_provider, 0);
    }

    #[test]
    fn test_model_selector_model_nav_wraps() {
        use crate::domain::models::provider::ModelDescriptor;
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "a".to_string(),
            display_name: "A".to_string(),
            healthy: true,
            models: vec![
                ModelDescriptor {
                    model_id: "m1".to_string(),
                    display_name: "M1".to_string(),
                    provider_id: "a".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                },
                ModelDescriptor {
                    model_id: "m2".to_string(),
                    display_name: "M2".to_string(),
                    provider_id: "a".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                },
            ],
        }];
        ms.selected_provider = 0;
        ms.selected_model = 0;
        ms.navigate_model(Direction::Up);
        assert_eq!(ms.selected_model, 1);
        ms.navigate_model(Direction::Down);
        assert_eq!(ms.selected_model, 0);
    }

    #[test]
    fn test_model_selector_single_provider_noop() {
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "a".to_string(),
            display_name: "A".to_string(),
            healthy: true,
            models: vec![],
        }];
        ms.selected_provider = 0;
        ms.navigate_provider(Direction::Left);
        assert_eq!(ms.selected_provider, 0);
        ms.navigate_provider(Direction::Right);
        assert_eq!(ms.selected_provider, 0);
    }

    #[test]
    fn test_model_selector_selected_returns_pair() {
        use crate::domain::models::provider::ModelDescriptor;
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "a".to_string(),
            display_name: "A".to_string(),
            healthy: true,
            models: vec![ModelDescriptor {
                model_id: "m1".to_string(),
                display_name: "M1".to_string(),
                provider_id: "a".to_string(),
                context_window: 200_000,
                capabilities: Default::default(),
                pricing_tier: None,
                stale: false,
            }],
        }];
        ms.selected_provider = 0;
        ms.selected_model = 0;
        let (pid, model) = ms.selected().unwrap();
        assert_eq!(pid, "a");
        assert_eq!(model.model_id, "m1");
    }

    #[test]
    fn test_model_selector_open_seeds_selection() {
        use crate::domain::models::provider::ModelDescriptor;
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "anthropic".to_string(),
            display_name: "Anthropic".to_string(),
            healthy: true,
            models: vec![
                ModelDescriptor {
                    model_id: "claude-opus".to_string(),
                    display_name: "Opus".to_string(),
                    provider_id: "anthropic".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                },
                ModelDescriptor {
                    model_id: "claude-sonnet".to_string(),
                    display_name: "Sonnet".to_string(),
                    provider_id: "anthropic".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                },
            ],
        }];
        ms.open(
            FocusState::Input,
            ms.columns.clone(),
            "anthropic",
            "claude-sonnet",
        );
        assert!(ms.active);
        assert_eq!(ms.selected_provider, 0);
        assert_eq!(ms.selected_model, 1);
    }

    #[test]
    fn test_selected_model_initializes_none() {
        let state = TuiState::new(80, 24);
        assert!(state.selected_model.is_none());
    }

    // Story 7.6 AC9 — model selector search/filter tests
    use crate::domain::models::provider::ModelDescriptor;

    fn make_test_selector() -> ModelSelectorState {
        let mut ms = ModelSelectorState::new();
        ms.columns = vec![ProviderColumn {
            provider_id: "openrouter".to_string(),
            display_name: "OpenRouter".to_string(),
            healthy: true,
            models: vec![
                ModelDescriptor {
                    model_id: "anthropic/claude-3.5-sonnet".to_string(),
                    display_name: "Claude 3.5 Sonnet".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 200_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                },
                ModelDescriptor {
                    model_id: "openai/gpt-4o".to_string(),
                    display_name: "GPT-4o".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 128_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                },
                ModelDescriptor {
                    model_id: "google/gemini-flash".to_string(),
                    display_name: "Gemini Flash ★".to_string(),
                    provider_id: "openrouter".to_string(),
                    context_window: 1_000_000,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                },
            ],
        }];
        ms
    }

    #[test]
    fn recompute_filter_finds_substring_in_model_id() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "gpt".to_string();
        ms.recompute_filter();
        assert_eq!(ms.filtered_indices, vec![1]);
    }

    #[test]
    fn recompute_filter_case_insensitive() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "CLAUde".to_string();
        ms.recompute_filter();
        assert_eq!(ms.filtered_indices, vec![0]);
    }

    #[test]
    fn recompute_filter_matches_display_name() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "flash".to_string();
        ms.recompute_filter();
        assert_eq!(ms.filtered_indices, vec![2]);
    }

    #[test]
    fn recompute_filter_unicode_in_display_name() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "★".to_string();
        ms.recompute_filter();
        assert_eq!(ms.filtered_indices, vec![2]);
    }

    #[test]
    fn recompute_filter_empty_query_returns_empty_indices() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = String::new();
        ms.recompute_filter();
        assert!(ms.filtered_indices.is_empty());
    }

    #[test]
    fn recompute_filter_no_match_returns_empty() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "zzzzz".to_string();
        ms.recompute_filter();
        assert!(ms.filtered_indices.is_empty());
    }

    #[test]
    fn column_switch_clears_search() {
        let mut ms = make_test_selector();
        ms.columns.push(ProviderColumn {
            provider_id: "anthropic".to_string(),
            display_name: "Anthropic".to_string(),
            healthy: true,
            models: vec![ModelDescriptor {
                model_id: "claude-sonnet".to_string(),
                display_name: "Claude Sonnet".to_string(),
                provider_id: "anthropic".to_string(),
                context_window: 200_000,
                capabilities: Default::default(),
                pricing_tier: None,
                stale: false,
            }],
        });
        ms.search_active = true;
        ms.search_query = "gpt".to_string();
        ms.filtered_indices = vec![1];
        ms.navigate_provider(Direction::Right);
        assert!(!ms.search_active);
        assert!(ms.search_query.is_empty());
        assert!(ms.filtered_indices.is_empty());
    }

    #[test]
    fn recompute_filter_clamps_selection_on_shrink() {
        let mut ms = make_test_selector();
        ms.search_active = true;
        ms.search_query = "a".to_string(); // matches all 3
        ms.recompute_filter();
        ms.selected_model = 2; // last index
        ms.search_query = "gpt".to_string(); // now only matches 1
        ms.recompute_filter();
        // After recompute, selected_model should be clamped to valid range
        assert_eq!(ms.selected_model, 0);
    }

    #[test]
    fn agent_panel_state_default_empty() {
        let state = AgentPanelState::default();
        assert!(state.cached_entries.is_empty());
        assert_eq!(state.selected_index, 0);
        assert!(state.drill_down_agent.is_none());
        assert!(state.pending_kill_confirm.is_none());
        assert!(state.spool_tail_cache.is_empty());
        assert_eq!(state.inspector_scroll_offset, 0);
    }

    #[test]
    fn selected_agent_clamps() {
        let mut tui = TuiState::new(80, 24);
        tui.agent_panel_state.selected_index = 7;
        assert!(tui.selected_agent().is_none());
    }

    #[test]
    fn which_key_chord_s_maps_to_panel_agents() {
        let wk = WhichKeyState::new();
        let action = wk.chord_map.get(&'s').expect("'s' chord missing");
        assert!(matches!(
            action,
            ChordAction::OpenPanel(crate::domain::models::visual::PanelType::Agents)
        ));
    }
}
