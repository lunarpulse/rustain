#![allow(dead_code)]
use std::collections::BTreeMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::domain::clock::{Clock, SystemClock};
use crate::domain::models::SessionMeta;
use crate::domain::models::conversation::{Conversation, generate_conversation_id};
use crate::domain::models::notice::FeedbackBlock;
use crate::domain::models::session::{SessionManager, SessionState};
use crate::domain::models::stream::StreamingState;
use crate::domain::models::view_state::ViewState;
use crate::domain::services::reducer::ReducerState;
use crate::domain::services::turn_queue::TurnQueue;

/// Unique identifier for a tab (TUI concept, not persisted).
pub type TabId = u64;

/// Identifier for a conversation (persisted, outlives the tab).
pub type ConversationId = String;

/// Per-tab state containing domain-level conversation and streaming data.
pub struct TabState {
    pub id: TabId,
    pub conversation: Conversation,
    pub streaming: StreamingState,
    /// Path B: authoritative reducer state (replaces `apply_chunk`).
    /// Populated in the event loop; `streaming` is a render mirror synced
    /// from this via `update_streaming_mirror` after each `reduce()` call.
    pub reducer: ReducerState,
    /// Per-tab view state for scroll anchoring, collapse policy, and
    /// summary tier (Story 16.3). Read by the render path in Story 16.4.
    pub view_state: ViewState,
    /// Injected clock for the reducer (testable via MockClock).
    pub clock: Arc<dyn Clock>,
    pub session: SessionManager,
    pub session_meta: SessionMeta,
    /// Scroll offset and auto-scroll are now read from `view_state.scroll_offset`
    /// and `view_state.mode` (Story 16.8, AC10). All sync sites in event_loop.rs
    /// write to view_state directly via `dispatch_view_scroll` and
    /// `reconcile_fold_toggle`; these legacy fields are deleted.
    pub block_boundaries: Vec<usize>,
    pub message_boundaries: Vec<usize>,
    pub user_message_boundaries: Vec<usize>,
    pub focused_tool_id: Option<String>,
    pub feedback_blocks: BTreeMap<String, FeedbackBlock>,
    pub active_feedback_id: Option<String>,
    pub total_content_height: usize,
    pub pending_anchor: Option<usize>,
    pub turn_queue: TurnQueue,
    pub turn_cancel: CancellationToken,
    /// Story 7.4: pending context carryover for fresh tab + summary injection.
    pub pending_context_carryover: Option<String>,
    /// Story 7.4: highest context-warning tier already surfaced on this tab.
    pub context_warn_level: crate::adapters::tui::state::ContextWarnLevel,
}

/// Current unix timestamp in milliseconds.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl TabState {
    /// Create a new TabState with a fresh conversation.
    ///
    /// Creates a detached CancellationToken (not linked to any parent).
    /// For production use, prefer `new_with_parent` which creates a child token.
    pub fn new(id: TabId) -> Self {
        Self::new_with_cancel(
            id,
            CancellationToken::new(),
            Arc::new(SystemClock::default()),
        )
    }

    /// Create a new TabState whose `turn_cancel` is a child of `session_cancel`.
    pub fn new_with_parent(id: TabId, session_cancel: &CancellationToken) -> Self {
        Self::new_with_cancel(
            id,
            session_cancel.child_token(),
            Arc::new(SystemClock::default()),
        )
    }

    /// Create a new TabState with an injected clock (test entrypoint).
    pub fn new_with_clock(id: TabId, clock: Arc<dyn Clock>) -> Self {
        Self::new_with_cancel(id, CancellationToken::new(), clock)
    }

    fn new_with_cancel(id: TabId, turn_cancel: CancellationToken, clock: Arc<dyn Clock>) -> Self {
        let now = now_unix();
        let instant_now = clock.now();
        let wall_anchor = now_unix_ms();
        let conversation = Conversation {
            id: generate_conversation_id(),
            title: String::new(),
            messages: Vec::new(),
            turns: Vec::new(),
            created_at: now,
            updated_at: now,
            last_response_at: None,
            session_id: Some(generate_conversation_id()),
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
        };
        let session_id = conversation.session_id.clone().unwrap_or_default();
        let session_meta = SessionMeta::from_conversation(&conversation);
        Self {
            id,
            conversation,
            streaming: StreamingState::default(),
            reducer: ReducerState::new(wall_anchor, instant_now),
            view_state: ViewState::default(),
            clock: clock.clone(),
            session: SessionManager::new(SessionState::Active { id: session_id }),
            session_meta,
            block_boundaries: Vec::new(),
            message_boundaries: Vec::new(),
            user_message_boundaries: Vec::new(),
            focused_tool_id: None,
            feedback_blocks: BTreeMap::new(),
            active_feedback_id: None,
            total_content_height: 0,
            pending_anchor: None,
            turn_queue: TurnQueue::default(),
            turn_cancel,
            pending_context_carryover: None,
            context_warn_level: crate::adapters::tui::state::ContextWarnLevel::None,
        }
    }

    /// Create a TabState from an existing conversation.
    ///
    /// Creates a detached CancellationToken (not linked to any parent).
    /// For production use, prefer `from_conversation_with_parent`.
    pub fn from_conversation(id: TabId, conversation: Conversation) -> Self {
        Self::from_conversation_with_cancel(
            id,
            conversation,
            CancellationToken::new(),
            Arc::new(SystemClock::default()),
        )
    }

    /// Create a TabState from an existing conversation with a child cancellation token.
    pub fn from_conversation_with_parent(
        id: TabId,
        conversation: Conversation,
        session_cancel: &CancellationToken,
    ) -> Self {
        Self::from_conversation_with_cancel(
            id,
            conversation,
            session_cancel.child_token(),
            Arc::new(SystemClock::default()),
        )
    }

    /// Create a TabState from an existing conversation with an injected clock (test entrypoint).
    pub fn from_conversation_with_clock(
        id: TabId,
        conversation: Conversation,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::from_conversation_with_cancel(id, conversation, CancellationToken::new(), clock)
    }

    fn from_conversation_with_cancel(
        id: TabId,
        conversation: Conversation,
        turn_cancel: CancellationToken,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let instant_now = clock.now();
        let wall_anchor = now_unix_ms();
        let session_id = conversation.session_id.clone().unwrap_or_default();
        let session_meta = SessionMeta::from_conversation(&conversation);
        Self {
            id,
            conversation,
            streaming: StreamingState::default(),
            reducer: ReducerState::new(wall_anchor, instant_now),
            view_state: ViewState::default(),
            clock: clock.clone(),
            session: SessionManager::new(SessionState::Active { id: session_id }),
            session_meta,
            block_boundaries: Vec::new(),
            message_boundaries: Vec::new(),
            user_message_boundaries: Vec::new(),
            focused_tool_id: None,
            feedback_blocks: BTreeMap::new(),
            active_feedback_id: None,
            total_content_height: 0,
            pending_anchor: None,
            turn_queue: TurnQueue::default(),
            turn_cancel,
            pending_context_carryover: None,
            context_warn_level: crate::adapters::tui::state::ContextWarnLevel::None,
        }
    }

    /// Reset TUI display state (on tab creation or conversation reset).
    pub fn reset_display_state(&mut self) {
        self.view_state.scroll_offset = 0;
        self.view_state.mode = crate::domain::models::AnchorMode::Following;
        self.block_boundaries.clear();
        self.message_boundaries.clear();
        self.user_message_boundaries.clear();
        self.focused_tool_id = None;
        self.total_content_height = 0;
        self.pending_anchor = None;
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Manages all open tabs in a single-threaded event loop.
/// No Arc/Mutex — lives exclusively in the event loop.
pub struct TabManager {
    tabs: Vec<TabState>,
    active_tab_index: usize,
    next_tab_id: TabId,
    session_cancel: CancellationToken,
}

impl TabManager {
    /// Create a new TabManager with one empty tab.
    pub fn new(session_cancel: CancellationToken) -> Self {
        let first_tab = TabState::new_with_parent(0, &session_cancel);
        Self {
            tabs: vec![first_tab],
            active_tab_index: 0,
            next_tab_id: 1,
            session_cancel,
        }
    }

    /// Create a new TabManager with a pre-existing conversation in the first tab.
    pub fn with_conversation(
        conversation: Conversation,
        session_cancel: CancellationToken,
    ) -> Self {
        let tab = TabState::from_conversation_with_parent(0, conversation, &session_cancel);
        Self {
            tabs: vec![tab],
            active_tab_index: 0,
            next_tab_id: 1,
            session_cancel,
        }
    }

    /// Create a new tab with a fresh conversation. Returns the new tab's ID.
    pub fn create_tab(&mut self) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = TabState::new_with_parent(id, &self.session_cancel);
        self.tabs.push(tab);
        self.active_tab_index = self.tabs.len() - 1;
        id
    }

    /// Create a new tab with a pre-existing conversation (for fork, open-from-sidebar, etc.).
    /// Returns the new tab's ID.
    pub fn create_tab_with_conversation(&mut self, conversation: Conversation) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = TabState::from_conversation_with_parent(id, conversation, &self.session_cancel);
        self.tabs.push(tab);
        self.active_tab_index = self.tabs.len() - 1;
        id
    }

    /// Reset the active tab's `turn_cancel` to a fresh child of `session_cancel`.
    /// Call this before each new turn so that a previously-cancelled tab can
    /// resume normal operation.
    pub fn reset_and_clone_turn_cancel(&mut self) -> CancellationToken {
        let session_cancel = self.session_cancel.clone();
        let tab = self.active_tab_mut();
        tab.turn_cancel = session_cancel.child_token();
        tab.turn_cancel.clone()
    }

    /// Close a tab by ID. Returns the closed tab's conversation for history storage.
    /// Cancels the tab's `turn_cancel` before extracting the conversation.
    /// If closing the last tab, creates a new empty tab automatically.
    /// Returns None if the tab ID was not found.
    pub fn close_tab(&mut self, id: TabId) -> Option<Conversation> {
        let pos = self.tabs.iter().position(|t| t.id == id)?;
        let tab = self.tabs.remove(pos);
        tab.turn_cancel.cancel();
        let conversation = tab.conversation;

        // Never run with zero tabs
        if self.tabs.is_empty() {
            let new_tab = TabState::new_with_parent(self.next_tab_id, &self.session_cancel);
            self.next_tab_id += 1;
            self.tabs.push(new_tab);
            self.active_tab_index = 0;
        } else {
            // Adjust active_tab_index: prefer next tab, fall back to previous
            self.active_tab_index = pos.min(self.tabs.len() - 1);
            if pos <= self.active_tab_index && self.active_tab_index > 0 {
                self.active_tab_index -= 1;
            }
        }

        Some(conversation)
    }

    /// Get an immutable reference to the active tab.
    pub fn active_tab(&self) -> &TabState {
        &self.tabs[self.active_tab_index]
    }

    /// Get a mutable reference to the active tab.
    pub fn active_tab_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_tab_index]
    }

    /// Switch to the next tab (wraps around).
    pub fn switch_to_next(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        // Clear thinking buffer on departing tab
        self.tabs[self.active_tab_index]
            .streaming
            .reset_thinking_buffer();
        self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
    }

    /// Switch to the previous tab (wraps around).
    pub fn switch_to_prev(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        // Clear thinking buffer on departing tab
        self.tabs[self.active_tab_index]
            .streaming
            .reset_thinking_buffer();
        self.active_tab_index = if self.active_tab_index == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab_index - 1
        };
    }

    /// Switch to a tab by 1-based index (number key 1-9). No-op if out of range.
    pub fn switch_to_index(&mut self, idx: usize) {
        if idx == 0 || idx > self.tabs.len() {
            return;
        }
        let new_idx = idx - 1;
        if new_idx == self.active_tab_index {
            return;
        }
        // Clear thinking buffer on departing tab
        self.tabs[self.active_tab_index]
            .streaming
            .reset_thinking_buffer();
        self.active_tab_index = new_idx;
    }

    /// Find a tab by conversation ID (immutable).
    pub fn find_by_conversation(&self, id: &str) -> Option<&TabState> {
        self.tabs.iter().find(|t| t.conversation.id == id)
    }

    /// Find a tab by conversation ID (mutable).
    pub fn find_by_conversation_mut(&mut self, id: &str) -> Option<&mut TabState> {
        self.tabs.iter_mut().find(|t| t.conversation.id == id)
    }

    /// Find a tab that has a pending tool invocation matching `tool_use_id`.
    /// Story 16.9 — used to route progress events to the correct tab.
    pub fn find_tab_with_pending_tool(&mut self, tool_use_id: &str) -> Option<&mut TabState> {
        self.tabs
            .iter_mut()
            .find(|t| t.reducer.pending_invocations.contains_key(tool_use_id))
    }

    /// Check whether the given tab_id is the currently active tab.
    pub fn is_active_tab(&self, tab_id: TabId) -> bool {
        self.tabs
            .get(self.active_tab_index)
            .is_some_and(|t| t.id == tab_id)
    }

    /// Total number of open tabs.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// All tabs (ordered, for rendering the tab bar).
    pub fn tabs(&self) -> &[TabState] {
        &self.tabs
    }

    /// The active tab's ID.
    pub fn active_tab_id(&self) -> TabId {
        self.tabs[self.active_tab_index].id
    }

    /// The active tab index (0-based, for tab bar highlight).
    pub fn active_tab_index(&self) -> usize {
        self.active_tab_index
    }
}

impl Default for TabManager {
    fn default() -> Self {
        Self::new(CancellationToken::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tab_manager_starts_with_one_tab() {
        let tm = TabManager::default();
        assert_eq!(tm.tab_count(), 1);
        assert_eq!(tm.active_tab_index(), 0);
    }

    #[test]
    fn test_create_tab_adds_and_focuses() {
        let mut tm = TabManager::default();
        let id = tm.create_tab();
        assert_eq!(tm.tab_count(), 2);
        assert_eq!(tm.active_tab_id(), id);
    }

    #[test]
    fn test_close_tab_removes_and_refocuses() {
        let mut tm = TabManager::default();
        let id1 = tm.active_tab_id();
        let _id2 = tm.create_tab();
        tm.close_tab(id1);
        assert_eq!(tm.tab_count(), 1);
    }

    #[test]
    fn test_close_last_tab_creates_new() {
        let mut tm = TabManager::default();
        let only_id = tm.active_tab_id();
        tm.close_tab(only_id);
        assert_eq!(tm.tab_count(), 1);
        assert_ne!(tm.active_tab_id(), only_id);
    }

    #[test]
    fn test_close_tab_cancels_turn_cancel() {
        let mut tm = TabManager::default();
        let id = tm.active_tab_id();
        let turn_cancel = tm.active_tab().turn_cancel.clone();
        assert!(!turn_cancel.is_cancelled());
        tm.close_tab(id);
        assert!(turn_cancel.is_cancelled());
    }

    #[test]
    fn test_switch_to_next_wraps() {
        let mut tm = TabManager::default();
        tm.create_tab();
        tm.create_tab();
        assert_eq!(tm.active_tab_index(), 2);
        tm.switch_to_next();
        assert_eq!(tm.active_tab_index(), 0);
    }

    #[test]
    fn test_switch_to_prev_wraps() {
        let mut tm = TabManager::default();
        tm.create_tab();
        tm.switch_to_prev();
        assert_eq!(tm.active_tab_index(), 0);
        tm.switch_to_prev();
        assert_eq!(tm.active_tab_index(), 1);
    }

    #[test]
    fn test_switch_to_index_1_based() {
        let mut tm = TabManager::default();
        tm.create_tab();
        tm.create_tab();
        tm.switch_to_index(1);
        assert_eq!(tm.active_tab_index(), 0);
        tm.switch_to_index(2);
        assert_eq!(tm.active_tab_index(), 1);
    }

    #[test]
    fn test_switch_to_index_out_of_range_noop() {
        let mut tm = TabManager::default();
        let orig_idx = tm.active_tab_index();
        tm.switch_to_index(9);
        assert_eq!(tm.active_tab_index(), orig_idx);
    }

    #[test]
    fn test_find_by_conversation() {
        let tm = TabManager::default();
        let conv_id = tm.active_tab().conversation.id.clone();
        assert!(tm.find_by_conversation(&conv_id).is_some());
        assert!(tm.find_by_conversation("nonexistent").is_none());
    }

    #[test]
    fn test_thinking_buffer_cleared_on_tab_switch() {
        let mut tm = TabManager::default();
        tm.create_tab();

        tm.active_tab_mut()
            .streaming
            .thinking_buffer
            .push_str("Let me think...");
        assert_eq!(
            tm.active_tab_mut().streaming.thinking_buffer,
            "Let me think..."
        );

        tm.switch_to_prev();
        assert_eq!(tm.tabs()[1].streaming.thinking_buffer, "");

        tm.tabs[tm.active_tab_index]
            .streaming
            .thinking_buffer
            .push_str("Another thought");
        tm.switch_to_next();
        assert_eq!(tm.tabs()[0].streaming.thinking_buffer, "");

        tm.create_tab();
        let cur = tm.active_tab_index();
        tm.tabs[cur]
            .streaming
            .thinking_buffer
            .push_str("Index thought");
        let next = if cur == 0 { 2 } else { 1 };
        tm.switch_to_index(next + 1);
        assert_eq!(tm.tabs()[cur].streaming.thinking_buffer, "");
    }

    #[test]
    fn test_new_with_parent_creates_child_token() {
        let session_cancel = CancellationToken::new();
        let tab = TabState::new_with_parent(0, &session_cancel);
        assert!(!tab.turn_cancel.is_cancelled());
        session_cancel.cancel();
        assert!(tab.turn_cancel.is_cancelled());
    }

    #[test]
    fn test_sibling_tabs_independent() {
        let session_cancel = CancellationToken::new();
        let session_cancel_ref = session_cancel.clone();
        let mut tm = TabManager::new(session_cancel);
        let id_a = tm.active_tab_id();
        let id_b = tm.create_tab();
        let cancel_a = tm
            .find_by_conversation(&tm.tabs()[0].conversation.id)
            .unwrap()
            .turn_cancel
            .clone();

        tm.switch_to_index(2);
        let cancel_b = tm.active_tab().turn_cancel.clone();

        cancel_a.cancel();
        assert!(cancel_a.is_cancelled());
        assert!(!cancel_b.is_cancelled());
        assert!(!session_cancel_ref.is_cancelled());
        let _ = id_a;
        let _ = id_b;
    }
}
