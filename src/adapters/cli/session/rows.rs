//! Shared pure decision-core for `session list` / `session delete` (Story 13.5a).
//!
//! `build_session_rows` is the single source of truth for filtering, sorting,
//! indexing, and default-resume marking. Story 13.5a-1 layers cross-workspace
//! merge logic on top without changing the single-workspace contract.

use std::cmp::Reverse;

use crate::domain::models::ConversationSummary;

/// Schema version for `rustain session list --json`.
pub const SESSION_LIST_SCHEMA_VERSION: &str = "1.1";

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

/// Cross-workspace wrapper. `SessionRow` stays byte-stable; the workspace address
/// lives alongside it for 13.5a-1 / 13.5b.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSessionRow {
    /// Canonical absolute workspace path.
    pub workspace: String,
    pub row: SessionRow,
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

/// Merge per-workspace rows into a single global listing.
pub fn build_all_workspace_rows(
    current_workspace: &str,
    per_workspace: Vec<(String, Vec<ConversationSummary>)>,
) -> Vec<WorkspaceSessionRow> {
    let mut rows: Vec<WorkspaceSessionRow> = per_workspace
        .into_iter()
        .flat_map(|(workspace, summaries)| {
            build_session_rows(summaries)
                .into_iter()
                .map(move |row| WorkspaceSessionRow {
                    workspace: workspace.clone(),
                    row,
                })
        })
        .collect();

    rows.sort_by(|a, b| {
        b.row
            .updated_at
            .cmp(&a.row.updated_at)
            .then_with(|| a.workspace.cmp(&b.workspace))
            .then_with(|| a.row.id.cmp(&b.row.id))
    });

    for (idx, row) in rows.iter_mut().enumerate() {
        row.row.index = idx + 1;
        row.row.is_default_resume = row.workspace == current_workspace && row.row.is_default_resume;
    }

    rows
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

    #[test]
    fn p0_1_all_workspace_rows_global_total_order() {
        let rows = build_all_workspace_rows(
            "/ws-b",
            vec![
                (
                    "/ws-a".to_string(),
                    vec![
                        summary("id-2", "Two", 300, 0, 1, false),
                        summary("id-1", "One", 300, 0, 1, false),
                    ],
                ),
                (
                    "/ws-b".to_string(),
                    vec![summary("id-3", "Three", 400, 0, 1, false)],
                ),
                (
                    "/ws-c".to_string(),
                    vec![summary("id-4", "Four", 300, 0, 1, false)],
                ),
            ],
        );

        let order: Vec<_> = rows
            .iter()
            .map(|row| (row.workspace.as_str(), row.row.id.as_str(), row.row.index))
            .collect();
        assert_eq!(
            order,
            vec![
                ("/ws-b", "id-3", 1),
                ("/ws-a", "id-1", 2),
                ("/ws-a", "id-2", 3),
                ("/ws-c", "id-4", 4),
            ]
        );
    }

    #[test]
    fn p0_2_all_workspace_excludes_per_workspace_empties() {
        let rows = build_all_workspace_rows(
            "/ws-a",
            vec![
                (
                    "/ws-a".to_string(),
                    vec![
                        summary("keep-a", "Keep A", 200, 0, 2, false),
                        summary("drop-a", "Drop A", 100, 0, 0, false),
                    ],
                ),
                (
                    "/ws-b".to_string(),
                    vec![
                        summary("drop-b", "Drop B", 300, 0, 0, false),
                        summary("keep-b", "Keep B", 150, 0, 1, false),
                    ],
                ),
            ],
        );

        let ids: Vec<_> = rows.iter().map(|row| row.row.id.as_str()).collect();
        assert_eq!(ids, vec!["keep-a", "keep-b"]);
        assert_eq!(rows[0].row.index, 1);
        assert_eq!(rows[1].row.index, 2);
    }

    #[test]
    fn p0_3_single_current_workspace_marker_under_all() {
        let rows = build_all_workspace_rows(
            "/ws-b",
            vec![
                (
                    "/ws-a".to_string(),
                    vec![summary("a1", "A1", 300, 0, 1, false)],
                ),
                (
                    "/ws-b".to_string(),
                    vec![
                        summary("b1", "B1", 200, 0, 1, false),
                        summary("b2", "B2", 100, 0, 1, false),
                    ],
                ),
                (
                    "/ws-c".to_string(),
                    vec![summary("c1", "C1", 400, 0, 1, false)],
                ),
            ],
        );

        let marked: Vec<_> = rows
            .iter()
            .filter(|row| row.row.is_default_resume)
            .map(|row| (row.workspace.as_str(), row.row.id.as_str()))
            .collect();
        assert_eq!(marked, vec![("/ws-b", "b1")]);
    }

    #[test]
    fn p0_3_zero_markers_when_current_workspace_not_listed() {
        let rows = build_all_workspace_rows(
            "/ws-missing",
            vec![
                (
                    "/ws-a".to_string(),
                    vec![summary("a1", "A1", 200, 0, 1, false)],
                ),
                (
                    "/ws-b".to_string(),
                    vec![summary("b1", "B1", 100, 0, 1, false)],
                ),
            ],
        );

        assert_eq!(
            rows.iter().filter(|row| row.row.is_default_resume).count(),
            0
        );
    }

    #[test]
    fn p0_4_id_tokens_preserved_through_merge() {
        let current_workspace = "/ws-a";
        let standalone = build_session_rows(vec![
            summary("keep-a", "Keep A", 200, 0, 2, false),
            summary("keep-b", "Keep B", 100, 0, 1, false),
        ]);
        let merged = build_all_workspace_rows(
            current_workspace,
            vec![(
                current_workspace.to_string(),
                vec![
                    summary("keep-a", "Keep A", 200, 0, 2, false),
                    summary("keep-b", "Keep B", 100, 0, 1, false),
                ],
            )],
        );

        let standalone_ids: Vec<_> = standalone.iter().map(|row| row.id.as_str()).collect();
        let merged_ids: Vec<_> = merged.iter().map(|row| row.row.id.as_str()).collect();
        assert_eq!(merged_ids, standalone_ids);
    }

    #[test]
    fn p0_5_same_id_in_two_workspaces_not_deduped() {
        let rows = build_all_workspace_rows(
            "/ws-a",
            vec![
                (
                    "/ws-a".to_string(),
                    vec![summary("same-id", "A", 200, 0, 1, false)],
                ),
                (
                    "/ws-b".to_string(),
                    vec![summary("same-id", "B", 100, 0, 1, false)],
                ),
            ],
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].row.id, "same-id");
        assert_eq!(rows[1].row.id, "same-id");
        assert_ne!(rows[0].workspace, rows[1].workspace);
    }
}
