//! Pure domain function for within-conversation substring search.
//!
//! Story 4.4 AC2. Case-insensitive substring match over `ChatMessage.content`,
//! returning byte-range matches mapped back to the **original** (non-lowercased)
//! message content so the adapter layer can highlight them in place without a
//! second UTF-8 pass.
//!
//! Unicode safety: uses `str::to_lowercase()` on both the query and each
//! message's content (NOT `eq_ignore_ascii_case` — lesson from Story 3-2 UTF-8
//! panics). Byte offsets are always valid char boundaries in the original
//! content.

use crate::domain::models::Conversation;

/// A single substring match inside a conversation.
///
/// Byte offsets are into the **original** `ChatMessage.content` string, not the
/// lowercased copy used for matching. Tool call inputs / results and feedback
/// blocks are NOT searched in v1 (story AC2 matching rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    /// Index into `Conversation.messages` — the message that contains the match.
    pub message_index: usize,
    /// Byte offset where the match starts in the original `content` string.
    pub byte_start: usize,
    /// Byte offset where the match ends (exclusive) in the original `content`.
    pub byte_end: usize,
}

/// Find every case-insensitive substring match of `query` in each message's
/// content.
///
/// Returns matches sorted by `(message_index, byte_start)` — the outer loop
/// iterates messages in order and `str::match_indices` emits matches in
/// ascending byte order.
///
/// Empty query, empty conversation, and "query longer than every message"
/// all return an empty vector.
pub fn find_matches(conversation: &Conversation, query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let query_lower: String = query.chars().flat_map(|c| c.to_lowercase()).collect();
    if query_lower.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for (msg_idx, msg) in conversation.messages.iter().enumerate() {
        find_matches_in_message(msg_idx, &msg.content, &query_lower, &mut matches);
    }

    for plan in conversation.plans.values() {
        let Some(host_msg_id) = &plan.host_message_id else {
            continue;
        };
        let Some(msg_idx) = conversation
            .messages
            .iter()
            .position(|m| &m.id == host_msg_id)
        else {
            continue;
        };

        let mut plan_text = plan.title.clone();
        for task in &plan.tasks {
            plan_text.push(' ');
            plan_text.push_str(&task.title);
            if !task.description.is_empty() {
                plan_text.push(' ');
                plan_text.push_str(&task.description);
            }
        }

        find_matches_in_message(msg_idx, &plan_text, &query_lower, &mut matches);
    }

    matches
}

/// Scan a single message's content for all case-insensitive substring matches
/// of `query_lower` (already lowercased by the caller).
///
/// Builds a lowercased mirror of the content along with a parallel char-boundary
/// map so byte offsets in the lowercased string can be translated back to
/// byte offsets in the original. Most chars lowercase to themselves at the same
/// byte offset (ASCII fast path), but a few Unicode chars (`İ` → `i\u{307}`,
/// German `ß` in some locales) change length — the boundary map handles both.
fn find_matches_in_message(
    msg_idx: usize,
    content: &str,
    query_lower: &str,
    out: &mut Vec<SearchMatch>,
) {
    let mut lower = String::with_capacity(content.len());
    // boundary_map[i] = (lower_byte_idx_at_char_start_i, orig_byte_idx_at_char_start_i)
    // Plus a trailing sentinel (lower.len(), content.len()) so end-of-match
    // offsets at the very end of the string can be translated.
    let mut boundary_map: Vec<(usize, usize)> = Vec::new();
    for (orig_byte_idx, ch) in content.char_indices() {
        boundary_map.push((lower.len(), orig_byte_idx));
        for lc in ch.to_lowercase() {
            lower.push(lc);
        }
    }
    boundary_map.push((lower.len(), content.len()));

    if query_lower.len() > lower.len() {
        return;
    }

    for (match_start_lower, _) in lower.match_indices(query_lower) {
        let match_end_lower = match_start_lower + query_lower.len();
        let orig_start = map_lower_to_orig(&boundary_map, match_start_lower);
        let orig_end = map_lower_to_orig(&boundary_map, match_end_lower);
        if let (Some(s), Some(e)) = (orig_start, orig_end) {
            out.push(SearchMatch {
                message_index: msg_idx,
                byte_start: s,
                byte_end: e,
            });
        }
        // If either lookup fails, the match landed inside a multi-byte char
        // whose lowercase form has a different length than the original —
        // rare edge case, skip without panicking.
    }
}

/// Binary-search-free linear lookup (boundary_map is tiny — one entry per
/// char in content, typically < 10k entries). For the ASCII fast path this
/// could be short-circuited but the overhead is negligible.
fn map_lower_to_orig(boundary_map: &[(usize, usize)], lower_idx: usize) -> Option<usize> {
    boundary_map
        .iter()
        .find(|(l, _)| *l == lower_idx)
        .map(|(_, o)| *o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::MessageRole;
    use crate::domain::models::conversation::{ChatMessage, Conversation};

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            id: "test-msg".to_string(),
            role,
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

    fn conv(messages: Vec<ChatMessage>) -> Conversation {
        Conversation {
            id: "conv-test".to_string(),
            title: "Test".to_string(),
            messages,
            turns: Vec::new(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
        }
    }

    #[test]
    fn empty_query_returns_empty() {
        let c = conv(vec![msg(MessageRole::User, "hello world")]);
        assert_eq!(find_matches(&c, ""), vec![]);
    }

    #[test]
    fn empty_conversation_returns_empty() {
        let c = conv(vec![]);
        assert_eq!(find_matches(&c, "hello"), vec![]);
    }

    #[test]
    fn single_message_single_match() {
        let c = conv(vec![msg(MessageRole::User, "hello world")]);
        let m = find_matches(&c, "hello");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].message_index, 0);
        assert_eq!(m[0].byte_start, 0);
        assert_eq!(m[0].byte_end, 5);
    }

    #[test]
    fn single_message_multi_match() {
        let c = conv(vec![msg(MessageRole::User, "foo bar foo baz foo")]);
        let m = find_matches(&c, "foo");
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].byte_start, 0);
        assert_eq!(m[0].byte_end, 3);
        assert_eq!(m[1].byte_start, 8);
        assert_eq!(m[1].byte_end, 11);
        assert_eq!(m[2].byte_start, 16);
        assert_eq!(m[2].byte_end, 19);
    }

    #[test]
    fn multi_message_multi_match() {
        let c = conv(vec![
            msg(MessageRole::User, "first hit"),
            msg(MessageRole::Assistant, "hit again"),
            msg(MessageRole::User, "no match here"),
            msg(MessageRole::Assistant, "third hit"),
        ]);
        let m = find_matches(&c, "hit");
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].message_index, 0);
        assert_eq!(m[1].message_index, 1);
        assert_eq!(m[2].message_index, 3);
    }

    #[test]
    fn case_insensitive_ascii() {
        let c = conv(vec![msg(MessageRole::User, "Hello WORLD")]);
        let m = find_matches(&c, "hello");
        assert_eq!(m.len(), 1);
        let content = &c.messages[0].content;
        assert_eq!(&content[m[0].byte_start..m[0].byte_end], "Hello");

        let m2 = find_matches(&c, "WORLD");
        assert_eq!(m2.len(), 1);
        assert_eq!(&content[m2[0].byte_start..m2[0].byte_end], "WORLD");

        let m3 = find_matches(&c, "wOrLd");
        assert_eq!(m3.len(), 1);
        assert_eq!(&content[m3[0].byte_start..m3[0].byte_end], "WORLD");
    }

    #[test]
    fn utf8_multi_byte_match_preserves_original_byte_offsets() {
        // "héllo" — é is U+00E9, 2 bytes (0xC3 0xA9) in UTF-8.
        // Total content: h(1) + é(2) + l(1) + l(1) + o(1) = 6 bytes.
        let c = conv(vec![msg(MessageRole::User, "héllo")]);
        let m = find_matches(&c, "HÉLLO");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].byte_start, 0);
        assert_eq!(m[0].byte_end, 6);
        // Round-trip: slicing the original content with these offsets
        // must yield valid UTF-8 and equal the original content.
        let content = &c.messages[0].content;
        assert_eq!(&content[m[0].byte_start..m[0].byte_end], "héllo");
    }

    #[test]
    fn utf8_substring_within_larger_content() {
        // Match "éll" inside "héllo world" — byte 1..5 in original.
        let c = conv(vec![msg(MessageRole::User, "héllo world")]);
        let m = find_matches(&c, "ÉLL");
        assert_eq!(m.len(), 1);
        let content = &c.messages[0].content;
        assert_eq!(&content[m[0].byte_start..m[0].byte_end], "éll");
    }

    #[test]
    fn query_longer_than_every_message_returns_empty() {
        let c = conv(vec![
            msg(MessageRole::User, "short"),
            msg(MessageRole::Assistant, "tiny"),
        ]);
        let m = find_matches(&c, "this query is much longer than any message");
        assert!(m.is_empty());
    }

    #[test]
    fn query_not_present_returns_empty() {
        let c = conv(vec![msg(MessageRole::User, "the quick brown fox")]);
        assert!(find_matches(&c, "xyzzy").is_empty());
    }

    #[test]
    fn matches_sorted_by_message_and_byte_start() {
        let c = conv(vec![
            msg(MessageRole::User, "aa bb aa"),
            msg(MessageRole::Assistant, "aa"),
        ]);
        let m = find_matches(&c, "aa");
        assert_eq!(m.len(), 3);
        assert!(m[0].message_index < m[1].message_index || m[0].byte_start < m[1].byte_start);
        assert_eq!(m[0].message_index, 0);
        assert_eq!(m[0].byte_start, 0);
        assert_eq!(m[1].message_index, 0);
        assert_eq!(m[1].byte_start, 6);
        assert_eq!(m[2].message_index, 1);
        assert_eq!(m[2].byte_start, 0);
    }

    #[test]
    fn overlapping_matches_return_non_overlapping() {
        // `match_indices` returns non-overlapping matches for substring searches.
        // "aaaa" searched for "aa" → 2 matches (at 0 and 2), NOT 3.
        let c = conv(vec![msg(MessageRole::User, "aaaa")]);
        let m = find_matches(&c, "aa");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].byte_start, 0);
        assert_eq!(m[1].byte_start, 2);
    }

    #[test]
    fn plan_task_title_search_returns_host_message() {
        use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus};

        let host_msg = ChatMessage {
            id: "msg-host".to_string(),
            role: MessageRole::Assistant,
            content: "Here is your plan.".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        };

        let mut plans = std::collections::HashMap::new();
        plans.insert(
            "plan-1".to_string(),
            Plan {
                id: "plan-1".to_string(),
                title: "Build feature".to_string(),
                tasks: vec![PlanTask {
                    number: 1,
                    title: "Write database migration".to_string(),
                    description: "Add new table".to_string(),
                    depends_on: vec![],
                    status: PlanTaskStatus::Pending,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                }],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 1_700_000_000,
                resolved_at: None,
                host_message_id: Some("msg-host".to_string()),
            },
        );

        let mut c = conv(vec![msg(MessageRole::User, "go ahead"), host_msg]);
        c.plans = plans;

        let m = find_matches(&c, "database migration");
        assert!(!m.is_empty(), "should find task title in plan");
        assert_eq!(m[0].message_index, 1, "match should point to host message");

        let m2 = find_matches(&c, "Build feature");
        assert!(!m2.is_empty(), "should find plan title");
        assert_eq!(m2[0].message_index, 1);

        let m3 = find_matches(&c, "Add new table");
        assert!(!m3.is_empty(), "should find task description");
        assert_eq!(m3[0].message_index, 1);
    }
}
