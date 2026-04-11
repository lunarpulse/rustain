#![allow(dead_code)]
use std::collections::BTreeMap;

use crate::domain::models::conversation::{Conversation, generate_conversation_id};
use crate::domain::models::notice::FeedbackBlock;
use crate::domain::models::session::{SessionManager, SessionState};
use crate::domain::models::stream::StreamingState;
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
    pub session: SessionManager,
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    /// Line offsets (from top) where each content block starts.
    pub block_boundaries: Vec<usize>,
    /// Line offsets (from top) where each user message starts.
    pub message_boundaries: Vec<usize>,
    /// Tool block id at the top of the viewport (for focus/keyboard interaction).
    pub focused_tool_id: Option<String>,
    /// Feedback blocks displayed in conversation, keyed by block ID.
    pub feedback_blocks: BTreeMap<String, FeedbackBlock>,
    /// The ID of the most recent active (actionable) feedback block.
    pub active_feedback_id: Option<String>,
    /// Total content height from last render.
    pub total_content_height: usize,
    /// Pending anchor message index for resize scroll preservation.
    pub pending_anchor: Option<usize>,
    /// Pending user messages queued between turns.
    pub turn_queue: TurnQueue,
}

impl TabState {
    /// Create a new TabState with a fresh conversation.
    pub fn new(id: TabId) -> Self {
        let now = now_unix();
        let conversation = Conversation {
            id: generate_conversation_id(),
            title: String::new(),
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
            last_response_at: None,
            session_id: Some(generate_conversation_id()),
            usage: None,
            fork_source: None,
        };
        let session_id = conversation.session_id.clone().unwrap_or_default();
        Self {
            id,
            conversation,
            streaming: StreamingState::default(),
            session: SessionManager::new(SessionState::Active { id: session_id }),
            scroll_offset: 0,
            auto_scroll: true,
            block_boundaries: Vec::new(),
            message_boundaries: Vec::new(),
            focused_tool_id: None,
            feedback_blocks: BTreeMap::new(),
            active_feedback_id: None,
            total_content_height: 0,
            pending_anchor: None,
            turn_queue: TurnQueue::default(),
        }
    }

    /// Create a TabState from an existing conversation.
    pub fn from_conversation(id: TabId, conversation: Conversation) -> Self {
        let session_id = conversation.session_id.clone().unwrap_or_default();
        Self {
            id,
            conversation,
            streaming: StreamingState::default(),
            session: SessionManager::new(SessionState::Active { id: session_id }),
            scroll_offset: 0,
            auto_scroll: true,
            block_boundaries: Vec::new(),
            message_boundaries: Vec::new(),
            focused_tool_id: None,
            feedback_blocks: BTreeMap::new(),
            active_feedback_id: None,
            total_content_height: 0,
            pending_anchor: None,
            turn_queue: TurnQueue::default(),
        }
    }

    /// Reset TUI display state (on tab creation or conversation reset).
    pub fn reset_display_state(&mut self) {
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.block_boundaries.clear();
        self.message_boundaries.clear();
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
}

impl TabManager {
    /// Create a new TabManager with one empty tab.
    pub fn new() -> Self {
        let first_tab = TabState::new(0);
        Self {
            tabs: vec![first_tab],
            active_tab_index: 0,
            next_tab_id: 1,
        }
    }

    /// Create a new TabManager with a pre-existing conversation in the first tab.
    pub fn with_conversation(conversation: Conversation) -> Self {
        let tab = TabState::from_conversation(0, conversation);
        Self {
            tabs: vec![tab],
            active_tab_index: 0,
            next_tab_id: 1,
        }
    }

    /// Create a new tab with a fresh conversation. Returns the new tab's ID.
    pub fn create_tab(&mut self) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = TabState::new(id);
        self.tabs.push(tab);
        self.active_tab_index = self.tabs.len() - 1;
        id
    }

    /// Close a tab by ID. Returns the closed tab's conversation for history storage.
    /// If closing the last tab, creates a new empty tab automatically.
    /// Returns None if the tab ID was not found.
    pub fn close_tab(&mut self, id: TabId) -> Option<Conversation> {
        let pos = self.tabs.iter().position(|t| t.id == id)?;
        let tab = self.tabs.remove(pos);
        let conversation = tab.conversation;

        // Never run with zero tabs
        if self.tabs.is_empty() {
            let new_tab = TabState::new(self.next_tab_id);
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
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tab_manager_starts_with_one_tab() {
        let tm = TabManager::new();
        assert_eq!(tm.tab_count(), 1);
        assert_eq!(tm.active_tab_index(), 0);
    }

    #[test]
    fn test_create_tab_adds_and_focuses() {
        let mut tm = TabManager::new();
        let id = tm.create_tab();
        assert_eq!(tm.tab_count(), 2);
        assert_eq!(tm.active_tab_id(), id);
    }

    #[test]
    fn test_close_tab_removes_and_refocuses() {
        let mut tm = TabManager::new();
        let id1 = tm.active_tab_id();
        let _id2 = tm.create_tab();
        tm.close_tab(id1);
        assert_eq!(tm.tab_count(), 1);
    }

    #[test]
    fn test_close_last_tab_creates_new() {
        let mut tm = TabManager::new();
        let only_id = tm.active_tab_id();
        tm.close_tab(only_id);
        assert_eq!(tm.tab_count(), 1);
        // New tab has a different ID
        assert_ne!(tm.active_tab_id(), only_id);
    }

    #[test]
    fn test_switch_to_next_wraps() {
        let mut tm = TabManager::new();
        tm.create_tab();
        tm.create_tab();
        // 3 tabs, active=2 (last)
        assert_eq!(tm.active_tab_index(), 2);
        tm.switch_to_next();
        assert_eq!(tm.active_tab_index(), 0); // wrapped
    }

    #[test]
    fn test_switch_to_prev_wraps() {
        let mut tm = TabManager::new();
        tm.create_tab();
        // 2 tabs, active=1
        tm.switch_to_prev();
        assert_eq!(tm.active_tab_index(), 0);
        tm.switch_to_prev();
        assert_eq!(tm.active_tab_index(), 1); // wrapped
    }

    #[test]
    fn test_switch_to_index_1_based() {
        let mut tm = TabManager::new();
        tm.create_tab();
        tm.create_tab();
        tm.switch_to_index(1);
        assert_eq!(tm.active_tab_index(), 0);
        tm.switch_to_index(2);
        assert_eq!(tm.active_tab_index(), 1);
    }

    #[test]
    fn test_switch_to_index_out_of_range_noop() {
        let mut tm = TabManager::new();
        let orig_idx = tm.active_tab_index();
        tm.switch_to_index(9);
        assert_eq!(tm.active_tab_index(), orig_idx);
    }

    #[test]
    fn test_find_by_conversation() {
        let tm = TabManager::new();
        let conv_id = tm.active_tab().conversation.id.clone();
        assert!(tm.find_by_conversation(&conv_id).is_some());
        assert!(tm.find_by_conversation("nonexistent").is_none());
    }
}
