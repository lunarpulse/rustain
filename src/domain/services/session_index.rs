//! In-memory session index for the sidebar.
//!
//! The `SessionIndex` maintains a sorted list of `SessionSummary` entries
//! and provides O(1) access by conversation ID via a HashMap index.
//! It lives in the single-threaded event loop (no Arc/Mutex needed).

use std::collections::HashMap;

/// Summary of a conversation for sidebar display.
/// Lightweight - no message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Conversation ID (unique, persisted).
    pub conversation_id: String,
    /// Display title.
    pub title: String,
    /// Unix timestamp (seconds) of last activity.
    pub updated_at: i64,
    /// Unix timestamp (seconds) when created.
    pub created_at: i64,
    /// Number of messages.
    pub message_count: usize,
    /// Whether this conversation is open in a tab.
    pub is_open: bool,
    /// Whether this conversation is the active tab.
    pub is_active: bool,
    /// Whether this conversation is a fork (Story 4-3a.1 / DF-095). Populated
    /// from `SessionMeta.fork_source.is_some()` at index-build time; the
    /// sidebar render path reads this flag directly (never touches disk).
    pub has_fork_source: bool,
}

impl SessionSummary {
    /// Create a new SessionSummary with default UI state.
    pub fn new(
        conversation_id: String,
        title: String,
        updated_at: i64,
        created_at: i64,
        message_count: usize,
    ) -> Self {
        Self {
            conversation_id,
            title,
            updated_at,
            created_at,
            message_count,
            is_open: false,
            is_active: false,
            has_fork_source: false,
        }
    }

    /// Create from a ConversationSummary (from StoragePort).
    pub fn from_conversation_summary(summary: &crate::domain::models::ConversationSummary) -> Self {
        Self {
            conversation_id: summary.id.clone(),
            title: summary.title.clone(),
            updated_at: summary.updated_at,
            created_at: summary.created_at,
            message_count: summary.message_count,
            is_open: false,
            is_active: false,
            has_fork_source: summary.has_fork_source,
        }
    }
}

/// In-memory index of all conversations for sidebar display.
/// Maintains entries sorted by `updated_at` descending (most recent first).
#[derive(Clone)]
pub struct SessionIndex {
    /// Sorted entries (newest first).
    entries: Vec<SessionSummary>,
    /// Map from conversation_id to index in entries.
    id_index: HashMap<String, usize>,
}

impl SessionIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            id_index: HashMap::new(),
        }
    }

    /// Build the index from a list of conversation summaries.
    /// Sorts by updated_at descending.
    pub fn build(summaries: Vec<crate::domain::models::ConversationSummary>) -> Self {
        let mut entries: Vec<SessionSummary> = summaries
            .into_iter()
            .map(|s| SessionSummary::from_conversation_summary(&s))
            .collect();

        // Sort by updated_at descending (most recent first)
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        let mut id_index = HashMap::with_capacity(entries.len());
        for (idx, entry) in entries.iter().enumerate() {
            id_index.insert(entry.conversation_id.clone(), idx);
        }

        Self { entries, id_index }
    }

    /// Get all entries (sorted by updated_at desc).
    pub fn entries(&self) -> &[SessionSummary] {
        &self.entries
    }

    /// Get a specific entry by conversation ID.
    ///
    /// `#[allow(dead_code)]`: public API consumed by tests; the binary currently
    /// reads entries via `entries()` and `get_mut()` only.
    #[allow(dead_code)]
    pub fn get(&self, conversation_id: &str) -> Option<&SessionSummary> {
        self.id_index
            .get(conversation_id)
            .and_then(|&idx| self.entries.get(idx))
    }

    /// Get a mutable reference to a specific entry.
    pub fn get_mut(&mut self, conversation_id: &str) -> Option<&mut SessionSummary> {
        self.id_index
            .get(conversation_id)
            .copied()
            .and_then(move |idx| self.entries.get_mut(idx))
    }

    /// "Touch" a conversation - update its timestamp and optionally title/message_count.
    /// Moves the entry to the front of the list (most recent).
    /// Returns true if a new entry was created.
    pub fn touch(
        &mut self,
        conversation_id: &str,
        title: Option<String>,
        message_count: Option<usize>,
    ) -> bool {
        let now = now_unix();

        if let Some(existing_idx) = self.id_index.get(conversation_id).copied() {
            // Entry exists - remove it and re-insert at front
            let mut entry = self.entries.remove(existing_idx);

            // Update fields
            entry.updated_at = now;
            if let Some(t) = title {
                entry.title = t;
            }
            if let Some(mc) = message_count {
                entry.message_count = mc;
            }

            // Insert at front
            self.entries.insert(0, entry);

            // Rebuild index
            self.rebuild_id_index();
            false
        } else {
            // Create new entry
            let title = title.unwrap_or_else(|| "New Conversation".to_string());
            let new_entry = SessionSummary::new(
                conversation_id.to_string(),
                title,
                now,
                now,
                message_count.unwrap_or(0),
            );
            self.entries.insert(0, new_entry);

            // Rebuild index
            self.rebuild_id_index();
            true
        }
    }

    /// Set the `is_open` flag for a conversation.
    pub fn set_open(&mut self, conversation_id: &str, is_open: bool) {
        if let Some(entry) = self.get_mut(conversation_id) {
            entry.is_open = is_open;
        }
    }

    /// Set the `is_active` flag for a conversation.
    /// Also clears the active flag from all other entries.
    pub fn set_active(&mut self, conversation_id: Option<&str>) {
        // Clear all active flags
        for entry in &mut self.entries {
            entry.is_active = false;
        }

        // Set the new active one
        if let Some(id) = conversation_id {
            if let Some(entry) = self.get_mut(id) {
                entry.is_active = true;
            }
        }
    }

    /// Remove an entry from the index.
    pub fn remove(&mut self, conversation_id: &str) -> Option<SessionSummary> {
        if let Some(idx) = self.id_index.get(conversation_id).copied() {
            let removed = self.entries.remove(idx);
            self.rebuild_id_index();
            Some(removed)
        } else {
            None
        }
    }

    /// Number of entries in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Rebuild the id_index from entries.
    fn rebuild_id_index(&mut self) {
        self.id_index.clear();
        self.id_index.reserve(self.entries.len());
        for (idx, entry) in self.entries.iter().enumerate() {
            self.id_index.insert(entry.conversation_id.clone(), idx);
        }
    }

    /// Clear all entries and the index.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.id_index.clear();
    }
}

impl Default for SessionIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn now_unix() -> i64 {
    crate::domain::models::session_meta::now_unix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::ConversationSummary;

    fn make_summary(id: &str, updated_at: i64) -> ConversationSummary {
        ConversationSummary {
            id: id.to_string(),
            title: format!("Conv {}", id),
            created_at: updated_at - 1000,
            updated_at,
            message_count: 5,
            has_fork_source: false,
        }
    }

    #[test]
    fn test_build_from_empty() {
        let index = SessionIndex::build(vec![]);
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn test_build_sorts_descending() {
        let summaries = vec![
            make_summary("a", 1000),
            make_summary("b", 3000),
            make_summary("c", 2000),
        ];
        let index = SessionIndex::build(summaries);

        assert_eq!(index.len(), 3);
        let entries = index.entries();
        assert_eq!(entries[0].conversation_id, "b"); // Most recent
        assert_eq!(entries[1].conversation_id, "c");
        assert_eq!(entries[2].conversation_id, "a"); // Oldest
    }

    #[test]
    fn test_touch_moves_to_front() {
        let summaries = vec![make_summary("a", 1000), make_summary("b", 3000)];
        let mut index = SessionIndex::build(summaries);

        // Touch "a" - it should move to front
        index.touch("a", None, Some(10));

        let entries = index.entries();
        assert_eq!(entries[0].conversation_id, "a");
        assert_eq!(entries[0].message_count, 10);
        assert_eq!(entries[1].conversation_id, "b");
    }

    #[test]
    fn test_touch_creates_new_entry() {
        let summaries = vec![make_summary("a", 1000)];
        let mut index = SessionIndex::build(summaries);

        // Touch a non-existent ID
        let created = index.touch("new", Some("New Title".to_string()), Some(3));

        assert!(created);
        assert_eq!(index.len(), 2);
        let entries = index.entries();
        assert_eq!(entries[0].conversation_id, "new");
        assert_eq!(entries[0].title, "New Title");
        assert_eq!(entries[0].message_count, 3);
    }

    #[test]
    fn test_set_open() {
        let summaries = vec![make_summary("a", 1000)];
        let mut index = SessionIndex::build(summaries);

        index.set_open("a", true);

        let entry = index.get("a").unwrap();
        assert!(entry.is_open);
    }

    #[test]
    fn test_set_active() {
        let summaries = vec![make_summary("a", 1000), make_summary("b", 2000)];
        let mut index = SessionIndex::build(summaries);

        index.set_active(Some("a"));

        let entry_a = index.get("a").unwrap();
        let entry_b = index.get("b").unwrap();
        assert!(entry_a.is_active);
        assert!(!entry_b.is_active);

        // Switch active
        index.set_active(Some("b"));
        let entry_a = index.get("a").unwrap();
        let entry_b = index.get("b").unwrap();
        assert!(!entry_a.is_active);
        assert!(entry_b.is_active);
    }

    #[test]
    fn test_set_active_none() {
        let summaries = vec![make_summary("a", 1000)];
        let mut index = SessionIndex::build(summaries);

        index.set_open("a", true);
        index.set_active(Some("a"));

        // Clear active
        index.set_active(None);

        let entry = index.get("a").unwrap();
        assert!(!entry.is_active);
    }

    #[test]
    fn test_remove() {
        let summaries = vec![make_summary("a", 1000), make_summary("b", 2000)];
        let mut index = SessionIndex::build(summaries);

        let removed = index.remove("a");

        assert!(removed.is_some());
        assert_eq!(removed.unwrap().conversation_id, "a");
        assert_eq!(index.len(), 1);
        assert!(index.get("a").is_none());
        assert!(index.get("b").is_some());
    }

    #[test]
    fn test_remove_nonexistent() {
        let summaries = vec![make_summary("a", 1000)];
        let mut index = SessionIndex::build(summaries);

        let removed = index.remove("nonexistent");

        assert!(removed.is_none());
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn test_get_by_id() {
        let summaries = vec![make_summary("a", 1000)];
        let index = SessionIndex::build(summaries);

        let entry = index.get("a");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().conversation_id, "a");

        let missing = index.get("nonexistent");
        assert!(missing.is_none());
    }

    #[test]
    fn test_clear() {
        let summaries = vec![make_summary("a", 1000)];
        let mut index = SessionIndex::build(summaries);

        index.clear();

        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
    }
}
