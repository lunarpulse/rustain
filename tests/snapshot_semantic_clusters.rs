#![allow(clippy::field_reassign_with_default)] // AI-12.1: test setup
//! Snapshot lock for the 8 representative semantic-label clusters from S16.4.5.
//!
//! Each entry is the visual realization of the labeler unit test of the same name
//! (see `summary_labeler.rs` `mod tests`). Re-record only when S16.4.5 changes
//! algorithm — see ADR-16-01 §Q3.
//!
//! # Snapshot format
//!
//! 8 collapsed Tier-2 turns rendered in one consolidated snapshot, each preceded
//! by a `# <cluster_name>` comment-row from a synthetic User message.
//! A single consolidated snapshot (not 8 separate ones) is the budget-conscious
//! choice — Epic 6 retro #2 prefers fewer, denser snapshots over many fragile ones.
//!
//! # Reviewing diffs
//!
//! When one cluster regresses, `cargo insta diff` shows the full lineup.
//! Reviewers should read the unified diff carefully — the diagnostic surface
//! is broader but the snapshot is cheaper to maintain than 8 separate files.

#[path = "common/render_helpers.rs"]
mod common;

use common::*;
use rustain::domain::clock::MockClock;
use rustain::domain::models::turn::PartId;
use rustain::domain::models::{ChatMessage, Conversation, ViewState};
use rustain::domain::models::{InvocationStatus, MessageRole, StopReason, SummaryTier, TurnPart};
use serde_json::json;

// ---------------------------------------------------------------------------
// Cluster fixture builders — mirror the 8 unit-test fixtures in
// summary_labeler.rs:1023-1230 verbatim
// ---------------------------------------------------------------------------

fn cluster_read_heavy_with_common_prefix() -> (&'static str, Vec<TurnPart>) {
    (
        "cluster_read_heavy_with_common_prefix",
        vec![
            TurnPart::ToolInvocation {
                id: PartId(0),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/auth/login.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(1),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/auth/jwt.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(2),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/auth/session.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(3),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/auth/csrf.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(4),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/auth/oauth.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
        ],
    )
}

fn cluster_mixed_kinds_with_separators() -> (&'static str, Vec<TurnPart>) {
    (
        "cluster_mixed_kinds_with_separators",
        vec![
            TurnPart::ToolInvocation {
                id: PartId(0),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/foo.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(1),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/bar.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(2),
                tool: "Bash".to_string(),
                args: json!({"command": "ls -la"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(3),
                tool: "Edit".to_string(),
                args: json!({"file_path": "src/auth/foo.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
        ],
    )
}

fn cluster_failure_containing_does_not_filter() -> (&'static str, Vec<TurnPart>) {
    (
        "cluster_failure_containing_does_not_filter",
        vec![
            TurnPart::ToolInvocation {
                id: PartId(0),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/a.rs"}),
                status: InvocationStatus::Error,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(1),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/b.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(2),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/c.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
        ],
    )
}

fn cluster_single_tool() -> (&'static str, Vec<TurnPart>) {
    (
        "cluster_single_tool",
        vec![TurnPart::ToolInvocation {
            id: PartId(0),
            tool: "Read".to_string(),
            args: json!({"file_path": "src/auth/login.rs"}),
            status: InvocationStatus::Success,
            started_at: 1_700_000_000_000,
            ended_at: Some(1_700_000_005_000),
        }],
    )
}

fn cluster_all_bash_no_path_qualifier() -> (&'static str, Vec<TurnPart>) {
    (
        "cluster_all_bash_no_path_qualifier",
        vec![
            TurnPart::ToolInvocation {
                id: PartId(0),
                tool: "Bash".to_string(),
                args: json!({"command": "ls -la"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(1),
                tool: "Bash".to_string(),
                args: json!({"command": "ls -la"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(2),
                tool: "Bash".to_string(),
                args: json!({"command": "ls -la"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(3),
                tool: "Bash".to_string(),
                args: json!({"command": "ls -la"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
        ],
    )
}

fn cluster_grep_only_with_path() -> (&'static str, Vec<TurnPart>) {
    (
        "cluster_grep_only_with_path",
        vec![
            TurnPart::ToolInvocation {
                id: PartId(0),
                tool: "Grep".to_string(),
                args: json!({"path": "src/auth/"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(1),
                tool: "Grep".to_string(),
                args: json!({"path": "src/auth/"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
        ],
    )
}

fn cluster_mixed_paths_no_common_prefix() -> (&'static str, Vec<TurnPart>) {
    (
        "cluster_mixed_paths_no_common_prefix",
        vec![
            TurnPart::ToolInvocation {
                id: PartId(0),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/foo.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(1),
                tool: "Read".to_string(),
                args: json!({"file_path": "tests/bar.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(2),
                tool: "Read".to_string(),
                args: json!({"file_path": "docs/baz.md"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
        ],
    )
}

fn cluster_parallel_calls_one_each() -> (&'static str, Vec<TurnPart>) {
    (
        "cluster_parallel_calls_one_each",
        vec![
            TurnPart::ToolInvocation {
                id: PartId(0),
                tool: "Read".to_string(),
                args: json!({"file_path": "src/a.rs"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(1),
                tool: "Bash".to_string(),
                args: json!({"command": "ls"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(2),
                tool: "Grep".to_string(),
                args: json!({"path": "src/"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
            TurnPart::ToolInvocation {
                id: PartId(3),
                tool: "Write".to_string(),
                args: json!({"file_path": "out.txt"}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            },
        ],
    )
}

// ---------------------------------------------------------------------------
// Consolidated cluster-lineup renderer
// ---------------------------------------------------------------------------

/// Build an 8-turn conversation where each turn is a collapsed Tier-2
/// assistant turn prefixed by a synthetic User message containing
/// `"# <cluster_name>"`. Render the conversation and return the text.
fn render_cluster_lineup(
    clusters: &[(&'static str, Vec<TurnPart>)],
    clock: &dyn rustain::domain::clock::Clock,
) -> String {
    let mut all_turns: Vec<rustain::domain::models::Turn> = Vec::new();
    let mut all_msgs: Vec<ChatMessage> = Vec::new();
    let mut msg_idx = 0u64;

    for (name, parts) in clusters {
        // Synthetic User message with the cluster name
        let uid = format!("cu{}", msg_idx);
        let umsg = ChatMessage {
            id: uid.clone(),
            role: MessageRole::User,
            content: format!("# {}", name),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: Default::default(),
            retracted_at_ms: None,
        };
        all_msgs.push(umsg);
        msg_idx += 1;

        // Collapsed assistant turn with the cluster parts
        let tid = format!("ct{}", msg_idx);
        let mut turn = rustain::domain::models::Turn::new("claude".into(), 1_700_000_000_000);
        turn.id = rustain::domain::models::TurnId(tid.clone());
        for part in parts.iter().cloned() {
            turn.push_part(|_id| part);
        }
        turn.stop_reason = Some(StopReason::EndTurn);

        let amsg = ChatMessage {
            id: tid,
            role: MessageRole::Assistant,
            content: String::new(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: rustain::domain::models::ChannelKind::Terminal,
            authorship: Default::default(),
            retracted_at_ms: None,
        };
        all_msgs.push(amsg);
        all_turns.push(turn);
        msg_idx += 1;
    }

    let conversation = Conversation {
        id: "cluster-conv".to_string(),
        title: "Cluster Snapshot".to_string(),
        messages: all_msgs,
        turns: all_turns,
        created_at: 1_700_000,
        updated_at: 1_700_000,
        last_response_at: Some(1_700_000),
        session_id: Some("cluster-session".to_string()),
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    };

    let mut vs = ViewState::default();
    vs.summary_tier = SummaryTier::Tier2;
    // Collapse all turns
    for turn in &conversation.turns {
        vs.collapsed.insert(turn.id.clone(), true);
    }

    render_to_string(&conversation, None, &vs, clock, 80, 100, None)
}

// ---------------------------------------------------------------------------
// Single consolidated snapshot test
// ---------------------------------------------------------------------------

#[test]
fn semantic_label_clusters_tier2_w80() {
    let clock = MockClock::at_wall_ms(1_700_000_000_000);

    let clusters: Vec<(&'static str, Vec<TurnPart>)> = vec![
        cluster_read_heavy_with_common_prefix(),
        cluster_mixed_kinds_with_separators(),
        cluster_failure_containing_does_not_filter(),
        cluster_single_tool(),
        cluster_all_bash_no_path_qualifier(),
        cluster_grep_only_with_path(),
        cluster_mixed_paths_no_common_prefix(),
        cluster_parallel_calls_one_each(),
    ];

    let text = render_cluster_lineup(&clusters, &clock);

    // AC5: in-test sanity assert — at least 3 of the 8 cluster lines
    // contain " in " (path-qualifier present). The failure-containing cluster
    // (#3) auto-expands due to UX-DR-FAILURE-AUTOEXPAND, so its collapsed
    // Tier-2 label is never rendered — count is 3, not 4. Guarding against
    // regression where labeler silently drops qualifiers.
    let path_qualifier_count = text.lines().filter(|l| l.contains(" in ")).count();
    assert!(
        path_qualifier_count >= 3,
        "Expected >=3 cluster lines with path qualifier ' in ', got {}. Text:\n{text}",
        path_qualifier_count,
    );

    insta::assert_snapshot!(text);
}
