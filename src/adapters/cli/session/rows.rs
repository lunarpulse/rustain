//! Shared pure decision-core for `session list` / `session delete` (Story 13.5a).
//!
//! `build_session_rows` is the single source of truth for filtering, sorting,
//! indexing, and default-resume marking. It is deterministic, disk-free, and
//! TTY-free so both render and future delete logic can reuse it.

use std::cmp::Reverse;

use crate::domain::models::ConversationSummary;

/// Schema version for `rustain session list --json`.
pub const SESSION_LIST_SCHEMA_VERSION: &str = "1.0";

/// One presentation row. Derived purely from a `ConversationSummary` + position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// 1-based; SNAPSHOT-RELATIVE presentation convenience, NOT a delete address (AC7).
    pub index: usize,
    /// The stable addressing contract (AC7) — 13.5b deletes by THIS.
    pub id: String,
    pub title: String,
    pub message_count: usize,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    /// Unix timestamp in seconds (== "last activity").
    pub updated_at: i64,
    /// Free from `ConversationSummary`; surfaced now to avoid a future schema bump.
    pub has_fork_source: bool,
    /// `(index == 1)`: what a bare `rustain` resumes. NOT `is_active` (no daemon claim).
    pub is_default_resume: bool,
}

/// Pure: filter empties, sort (total order), assign 1-based indices, mark the
/// most-recent surviving row as the default-resume target.
///
/// No disk, no TTY, no re-dedup. `list_conversations` already deduplicates;
/// a second dedup here would be a phantom seam (AI-11.1).
pub fn build_session_rows(summaries: Vec<ConversationSummary>) -> Vec<SessionRow> {
    let mut filtered: Vec<ConversationSummary> = summaries
        .into_iter()
        .filter(|s| s.message_count > 0)
        .collect();

    // Total order: most-recent first, tie-broken by id ascending. Defensive
    // re-sort so the index contract is deterministic regardless of the
    // `StoragePort` implementation or caller ordering.
    filtered.sort_by_key(|s| (Reverse(s.updated_at), s.id.clone()));

    filtered
        .into_iter()
        .enumerate()
        .map(|(idx, s)| {
            let index = idx + 1;
            SessionRow {
                index,
                id: s.id,
                title: s.title,
                message_count: s.message_count,
                created_at: s.created_at,
                updated_at: s.updated_at,
                has_fork_source: s.has_fork_source,
                is_default_resume: index == 1,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(
        id: &str,
        title: &str,
        updated_at: i64,
        created_at: i64,
        message_count: usize,
        has_fork_source: bool,
    ) -> ConversationSummary {
        ConversationSummary {
            id: id.to_string(),
            title: title.to_string(),
            created_at,
            updated_at,
            message_count,
            has_fork_source,
        }
    }

    #[test]
    fn p0_1_all_fields_row_mapping() {
        let rows = build_session_rows(vec![
            summary("a", "First", 100, 10, 3, false),
            summary("b", "Second", 200, 20, 1, true),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].index, 1);
        assert_eq!(rows[0].id, "b");
        assert_eq!(rows[0].title, "Second");
        assert_eq!(rows[0].message_count, 1);
        assert_eq!(rows[0].created_at, 20);
        assert_eq!(rows[0].updated_at, 200);
        assert!(rows[0].has_fork_source);
        assert!(rows[0].is_default_resume);

        assert_eq!(rows[1].index, 2);
        assert_eq!(rows[1].id, "a");
        assert!(!rows[1].is_default_resume);
    }

    #[test]
    fn p0_2_sort_most_recent_first() {
        let rows = build_session_rows(vec![
            summary("a", "A", 100, 0, 1, false),
            summary("b", "B", 300, 0, 1, false),
            summary("c", "C", 200, 0, 1, false),
        ]);
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c", "a"]);
    }

    #[test]
    fn p0_3_empty_session_exclusion() {
        let rows = build_session_rows(vec![
            summary("empty", "Empty", 300, 0, 0, false),
            summary("kept", "Kept", 200, 0, 1, false),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "kept");
        assert_eq!(rows[0].index, 1);
    }

    #[test]
    fn p0_4_default_resume_marker() {
        let rows = build_session_rows(vec![
            summary("older", "Older", 100, 0, 1, false),
            summary("newer", "Newer", 200, 0, 1, false),
        ]);
        assert_eq!(rows.iter().filter(|r| r.is_default_resume).count(), 1);
        assert!(rows[0].is_default_resume);
        assert_eq!(rows[0].id, "newer");
    }

    #[test]
    fn p0_4_default_resume_empty_list() {
        let rows: Vec<SessionRow> = build_session_rows(vec![]);
        assert!(rows.is_empty());
    }

    #[test]
    fn p0_4_default_resume_single_row() {
        let rows = build_session_rows(vec![summary("only", "Only", 100, 0, 1, false)]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_default_resume);
    }

    #[test]
    fn p0_5_is_default_resume_reflects_most_recent() {
        // Conceptually a daemon could hold "held", but offline CLI can only
        // know the default-resume target = most-recent.
        let rows = build_session_rows(vec![
            summary("held", "Daemon-held older", 100, 0, 1, false),
            summary("recent", "Most recent", 200, 0, 1, false),
        ]);
        assert!(rows[0].is_default_resume);
        assert_eq!(rows[0].id, "recent");
        assert!(!rows[1].is_default_resume);
    }

    #[test]
    fn p0_6_total_order_tie_break_by_id() {
        // Same updated_at, shuffled input order — output must be deterministic
        // (id ascending) so indices are stable across runs.
        let rows = build_session_rows(vec![
            summary("z", "Z", 100, 0, 1, false),
            summary("a", "A", 100, 0, 1, false),
            summary("m", "M", 100, 0, 1, false),
        ]);
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "m", "z"]);
        assert!(rows[0].is_default_resume);
        assert_eq!(rows[0].index, 1);
    }
}
