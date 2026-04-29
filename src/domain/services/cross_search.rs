//! Cross-conversation search (Story 4-4 AC5).
//!
//! Iterates over `SessionIndex.entries()` in recency order, loading each
//! conversation via `StoragePort::load_conversation` and reusing the
//! within-conversation `find_matches` engine to scan content.
//!
//! Bounded by `CrossSearchBudget` (20 results / 200 ms wall-clock, whichever
//! comes first). Truncation reason (count vs time) is signaled back to the
//! caller via `CrossSearchOutcome.truncated_by_count` / `.truncated_by_time`
//! so the overlay can show a specific hint (reviewer Fix 6).
//!
//! Takes a `&dyn StoragePort` as an argument — stays in the domain layer
//! because it depends only on the port trait, never on a concrete adapter.

use std::time::{Duration, Instant};

use crate::domain::models::shorten_text;
use crate::domain::ports::StoragePort;
use crate::domain::services::search::find_matches;
use crate::domain::services::session_index::SessionIndex;

/// One result row in the cross-search overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossSearchResult {
    pub conversation_id: String,
    pub title: String,
    /// 25-char context window centered on the first match, char-boundary
    /// clamped per AC5 vertical-stack layout.
    pub excerpt: String,
    pub timestamp: i64,
    pub first_match_message_index: usize,
}

/// Resource budget for a cross-search scan.
#[derive(Debug, Clone, Copy)]
pub struct CrossSearchBudget {
    pub max_results: usize,
    pub max_duration: Duration,
}

impl Default for CrossSearchBudget {
    fn default() -> Self {
        Self {
            max_results: 20,
            max_duration: Duration::from_millis(200),
        }
    }
}

/// Outcome of a cross-search scan — results plus truncation reason.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CrossSearchOutcome {
    pub results: Vec<CrossSearchResult>,
    pub truncated_by_count: bool,
    pub truncated_by_time: bool,
    /// Total conversations actually scanned (for honest truncation copy).
    pub scanned: usize,
    /// Total conversations in the index at scan time.
    pub total: usize,
}

/// Run cross-conversation search with the given budget.
///
/// Returns a `CrossSearchOutcome` containing the result list (up to
/// `budget.max_results` entries) and truncation flags for UI signaling.
/// Never panics on I/O errors — failed `load_conversation` calls are skipped
/// and logged via `tracing`, letting the scan continue.
pub async fn run_cross_search(
    storage: &dyn StoragePort,
    index: &SessionIndex,
    query: &str,
    budget: CrossSearchBudget,
) -> CrossSearchOutcome {
    let mut outcome = CrossSearchOutcome {
        total: index.entries().len(),
        ..Default::default()
    };

    if query.is_empty() {
        return outcome;
    }

    let start = Instant::now();

    for entry in index.entries() {
        // Time-limit check BEFORE scanning the next conversation so we don't
        // start an expensive load right at the edge of the budget.
        if start.elapsed() >= budget.max_duration {
            outcome.truncated_by_time = true;
            break;
        }
        outcome.scanned += 1;

        match storage.load_conversation(&entry.conversation_id).await {
            Ok(Some(conv)) => {
                let matches = find_matches(&conv, query);
                if let Some(first) = matches.first() {
                    let msg_content = conv
                        .messages
                        .get(first.message_index)
                        .map(|m| m.content.as_str())
                        .unwrap_or("");
                    let excerpt = build_excerpt(msg_content, first.byte_start, 25);
                    outcome.results.push(CrossSearchResult {
                        conversation_id: conv.id.clone(),
                        title: if conv.title.is_empty() {
                            "(untitled)".to_string()
                        } else {
                            conv.title.clone()
                        },
                        excerpt,
                        timestamp: conv.updated_at,
                        first_match_message_index: first.message_index,
                    });
                    if outcome.results.len() >= budget.max_results {
                        outcome.truncated_by_count = true;
                        break;
                    }
                }
            }
            Ok(None) => {
                // Conversation missing — skip silently (stale index entry).
            }
            Err(e) => {
                tracing::warn!(
                    "cross_search: load_conversation({}) failed: {}",
                    entry.conversation_id,
                    e
                );
            }
        }
    }

    outcome
}

/// Build a 25-char window around `byte_pos` from the original message content.
///
/// Char-boundary safe — walks the string via `char_indices` to find a window
/// of `window_chars` characters centered on `byte_pos`. If the window runs
/// off either end, it's clamped to the string bounds. Leading/trailing
/// "…" ellipses are added to indicate truncation.
fn build_excerpt(content: &str, byte_pos: usize, window_chars: usize) -> String {
    if content.is_empty() {
        return String::new();
    }
    let char_count = content.chars().count();
    if char_count <= window_chars {
        return shorten_text(content, window_chars).to_string();
    }

    // Find the char index corresponding to byte_pos.
    let center_char_idx = content
        .char_indices()
        .enumerate()
        .find(|(_, (b, _))| *b >= byte_pos)
        .map(|(c, _)| c)
        .unwrap_or(0);

    let half = window_chars / 2;
    let start_char = center_char_idx.saturating_sub(half);
    let end_char = (start_char + window_chars).min(char_count);
    let real_start = end_char.saturating_sub(window_chars);

    let chars: String = content
        .chars()
        .skip(real_start)
        .take(end_char - real_start)
        .collect();

    let mut excerpt = String::new();
    if real_start > 0 {
        excerpt.push('…');
    }
    excerpt.push_str(&chars);
    if end_char < char_count {
        excerpt.push('…');
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::errors::StorageError;
    use crate::domain::models::{ChatMessage, Conversation, ConversationSummary, MessageRole};
    use crate::domain::ports::StoragePort;

    /// Minimal StoragePort stub for testing cross_search.
    ///
    /// Only `load_conversation` is functional; other methods return default
    /// or stub values. The test builds a fixed set of conversations and
    /// returns them by id.
    struct StubStorage {
        conversations: std::collections::HashMap<String, Conversation>,
    }

    #[async_trait::async_trait]
    impl StoragePort for StubStorage {
        async fn save_conversation(&self, _c: &Conversation) -> Result<(), StorageError> {
            Ok(())
        }
        async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError> {
            Ok(self.conversations.get(id).cloned())
        }
        async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError> {
            Ok(self
                .conversations
                .values()
                .map(|c| ConversationSummary {
                    id: c.id.clone(),
                    title: c.title.clone(),
                    created_at: c.created_at,
                    updated_at: c.updated_at,
                    message_count: c.messages.len(),
                    has_fork_source: false,
                })
                .collect())
        }
        async fn delete_conversation(&self, _id: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn save_session_meta(
            &self,
            _id: &str,
            _meta: &crate::domain::models::SessionMeta,
        ) -> Result<(), StorageError> {
            Ok(())
        }
        async fn load_session_meta(
            &self,
            _id: &str,
        ) -> Result<Option<crate::domain::models::SessionMeta>, StorageError> {
            Ok(None)
        }
    }

    fn msg(content: &str) -> ChatMessage {
        ChatMessage {
            id: "m".to_string(),
            role: MessageRole::User,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        }
    }

    fn conv(id: &str, title: &str, content: &str, updated_at: i64) -> Conversation {
        Conversation {
            id: id.to_string(),
            title: title.to_string(),
            messages: vec![msg(content)],
            turns: Vec::new(),
            created_at: updated_at,
            updated_at,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        }
    }

    fn stub_with(convs: Vec<Conversation>) -> (StubStorage, SessionIndex) {
        let mut map = std::collections::HashMap::new();
        let summaries: Vec<ConversationSummary> = convs
            .iter()
            .map(|c| ConversationSummary {
                id: c.id.clone(),
                title: c.title.clone(),
                created_at: c.created_at,
                updated_at: c.updated_at,
                message_count: c.messages.len(),
                has_fork_source: false,
            })
            .collect();
        for c in convs {
            map.insert(c.id.clone(), c);
        }
        let index = SessionIndex::build(summaries);
        (StubStorage { conversations: map }, index)
    }

    #[tokio::test]
    async fn empty_query_returns_empty() {
        let (storage, index) = stub_with(vec![conv("a", "t", "hello", 100)]);
        let outcome = run_cross_search(&storage, &index, "", CrossSearchBudget::default()).await;
        assert!(outcome.results.is_empty());
        assert_eq!(outcome.total, 1);
        assert_eq!(outcome.scanned, 0);
    }

    #[tokio::test]
    async fn single_matching_conversation_returns_one_result() {
        let (storage, index) = stub_with(vec![
            conv("a", "alpha", "nothing here", 100),
            conv("b", "beta", "here is postgres today", 200),
            conv("c", "gamma", "also nothing", 300),
        ]);
        let outcome =
            run_cross_search(&storage, &index, "postgres", CrossSearchBudget::default()).await;
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].conversation_id, "b");
        assert!(outcome.results[0].excerpt.contains("postgres"));
        assert!(!outcome.truncated_by_count);
        assert!(!outcome.truncated_by_time);
    }

    #[tokio::test]
    async fn truncation_by_count_sets_flag() {
        // 25 conversations all matching, budget = 20 → truncated_by_count
        let convs: Vec<Conversation> = (0..25)
            .map(|i| conv(&format!("c{}", i), &format!("t{}", i), "match", 1000 - i))
            .collect();
        let (storage, index) = stub_with(convs);
        let outcome =
            run_cross_search(&storage, &index, "match", CrossSearchBudget::default()).await;
        assert_eq!(outcome.results.len(), 20);
        assert!(outcome.truncated_by_count);
    }

    #[tokio::test]
    async fn no_match_returns_empty_results_not_truncated() {
        let (storage, index) = stub_with(vec![
            conv("a", "t1", "nothing matches", 100),
            conv("b", "t2", "still nothing", 200),
        ]);
        let outcome =
            run_cross_search(&storage, &index, "xyzzy", CrossSearchBudget::default()).await;
        assert!(outcome.results.is_empty());
        assert_eq!(outcome.scanned, 2);
        assert!(!outcome.truncated_by_count);
        assert!(!outcome.truncated_by_time);
    }

    #[test]
    fn build_excerpt_short_content_returns_whole_content() {
        assert_eq!(build_excerpt("hi", 0, 25), "hi");
    }

    #[test]
    fn build_excerpt_long_content_clips_and_adds_ellipses() {
        let content = "the quick brown fox jumps over the lazy dog several times every day";
        let pos = content.find("fox").unwrap();
        let excerpt = build_excerpt(content, pos, 25);
        assert!(excerpt.contains("fox"));
        assert!(excerpt.starts_with('…') || excerpt.starts_with("the"));
    }

    #[test]
    fn build_excerpt_handles_utf8_boundary() {
        let content = "prefix héllo world suffix";
        let pos = content.find("héllo").unwrap();
        let excerpt = build_excerpt(content, pos, 15);
        assert!(excerpt.contains("héllo"));
    }
}
