//! Heuristic semantic labeler for collapsed-turn one-liners.
//!
//! Produces a [`SummaryLabel`] with two tiers:
//! - **Tier-1** (cheap, always available): `"N tools"` + optional elapsed time.
//!   Peer-equivalent to Claude Code / codex / gemini-cli / opencode.
//! - **Tier-2** (semantic, durable rustain differentiator per UX-DR-COLLAPSED-TIER):
//!   path-prefix clustered summary like `"3 reads in src/auth/, 1 grep"`.
//!   Toggled globally via the `zs` keymap (Story 16.6).
//!
//! # Algorithm (Tier-2)
//!
//! 1. Filter `turn.parts` to `TurnPart::ToolInvocation` only (Prose / ToolResult / Reasoning skipped).
//! 2. Group by `tool.to_lowercase()` (canonical kind across PascalCase / snake_case / MCP names).
//! 3. Within each cluster, compute longest-common-path-prefix across all path-bearing args.
//!    Path keys probed in priority order: `file_path` → `filePath` → `path`.
//! 4. Trim LCP back to last `/` boundary; drop qualifier if empty or `"/"` (trivial).
//! 5. Sort clusters by `(Reverse(count), kind_alphabetical)` — descending count primary, alphabetical tiebreak.
//! 6. Format each cluster as `"<count> <verb>"` (singular for count=1, plural for count>=2; irregular: `bash → bashes`).
//!    Append `" in <prefix>"` only when count >= 2 AND LCP non-empty AND non-`"/"`.
//! 7. Cap at 4 clusters; trailing `, +N more` for overflow (the 5th and beyond collapse).
//! 8. Self-bound the assembled string at 120 chars; drop trailing clusters and append U+2026 if exceeded.
//!
//! # Why self-bound at 120 chars (not terminal-width-aware)
//!
//! Render layer (`chat_pane::render_collapsed_turn`) applies its own width-aware truncation to the
//! COMPOSED collapsed line (gutter + glyph + first-prose-sentence + `·` + tier-text + `✓`). The labeler
//! self-bounds Tier-2 to 120 chars to keep the worst case under control without coupling the labeler
//! to terminal width — a clean separation per the S16.4 contract.
//!
//! # Out of scope (ADR-16-01 §Q3)
//!
//! - **LLM polish**: trait-gated, off by default, cached by invocation-set hash. Activation criteria
//!   deferred until heuristic clusterer (this story) ships AND telemetry from dogfooding shows where
//!   the heuristic falls short. See ADR-16-01 §Q3 for the activation rubric.
//!
//! # API stability
//!
//! [`SummaryLabel`] (`{ tier1, tier2 }`) and [`compute_summary_label`] (`(turn, elapsed_ms) -> SummaryLabel`)
//! signatures are locked from S16.4. Future changes (LLM polish, width-aware truncation, custom verb
//! tables) extend without breaking the existing call site at `chat_pane::render_collapsed_turn`.

use crate::domain::models::turn::{Turn, TurnPart};
use std::collections::BTreeMap;

/// Tier-1 and Tier-2 summary strings for a completed turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryLabel {
    /// Cheap-form summary (e.g. "3 tools, 12.5s").
    pub tier1: String,
    /// Semantic-form summary (e.g. "3 reads in src/auth/, 1 grep").
    pub tier2: String,
}

/// Compute the collapsed-summary label for a completed turn.
///
/// `elapsed_ms`: wall-clock elapsed for the turn as a whole
/// (`Some(ms)` if a wall-anchor is available, `None` otherwise — sentinel 0
/// propagates through as `None` per P0-8 decision).
pub fn compute_summary_label(turn: &Turn, elapsed_ms: Option<i64>) -> SummaryLabel {
    let n = turn
        .parts
        .iter()
        .filter(|p| matches!(p, TurnPart::ToolInvocation { .. }))
        .count();
    let elapsed_suffix = match elapsed_ms {
        Some(ms) if ms > 0 => format!(", {:.1}s", ms as f64 / 1000.0),
        _ => String::new(),
    };
    let tier1 = format!(
        "{} tool{}{}",
        n,
        if n == 1 { "" } else { "s" },
        elapsed_suffix
    );

    let clusters = cluster_invocations(&turn.parts);
    let tier2 = if clusters.is_empty() {
        tier1.clone()
    } else {
        assemble_tier2(&clusters)
    };

    SummaryLabel { tier1, tier2 }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Normalize a tool name to its canonical lowercase form.
///
/// Handles PascalCase (`"Read"`), snake_case (`"read"`), and MCP names
/// (`"mcp__filesystem__read_text_file"`) without an allowlist.
fn canonical_tool_kind(tool: &str) -> String {
    tool.to_lowercase()
}

/// Emit the verb form for a given tool kind and instance count.
///
/// - `count == 1` → return `kind` as-is (singular).
/// - `count >= 2` → check irregulars table first (`"bash" => "bashes"`),
///   otherwise append `"s"`.
fn pluralize_verb(kind: &str, count: usize) -> String {
    if count == 1 {
        return kind.to_string();
    }
    if kind == "bash" {
        return "bashes".to_string();
    }
    format!("{}s", kind)
}

/// Extract a file-path string from a tool invocation's `args` JSON.
///
/// Probes keys in priority order: `file_path` → `filePath` → `path`.
/// Returns `Some(&str)` for the first non-empty string value, or `None`
/// if no recognized key contains a non-empty string.
fn extract_path_from_args(args: &serde_json::Value) -> Option<&str> {
    for key in &["file_path", "filePath", "path"] {
        if let Some(val) = args.get(key) {
            if let Some(s) = val.as_str() {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// Compute the longest common path prefix across a set of paths.
///
/// Algorithm:
/// 1. Byte-by-byte LCP across all paths (bytes, since paths are UTF‑8).
/// 2. Trim back to the last `/` boundary so the prefix never ends mid-filename.
/// 3. If the trimmed result is empty OR equals `"/"` → return empty `String`
///    (caller decides whether to emit a qualifier).
///
/// Edge cases:
/// - 0 paths → empty
/// - 1 path  → empty (caller drops qualifier when count == 1 anyway)
/// - All-equal paths → return path trimmed to last `/`
fn longest_common_path_prefix(paths: &[&str]) -> String {
    if paths.len() < 2 {
        return String::new();
    }

    let first = paths[0];
    let mut byte_idx = 0;
    for ch in first.chars() {
        let ch_len = ch.len_utf8();
        let slice = first.get(byte_idx..byte_idx + ch_len);
        if paths
            .iter()
            .all(|p| p.get(byte_idx..byte_idx + ch_len) == slice)
        {
            byte_idx += ch_len;
        } else {
            break;
        }
    }

    let lcp = &first[..byte_idx];
    // Trim back to last '/' boundary
    match lcp.rfind('/') {
        Some(pos) => {
            let trimmed = &lcp[..=pos]; // inclusive
            if trimmed == "/" || trimmed.is_empty() {
                String::new()
            } else {
                trimmed.to_string()
            }
        }
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Clustering
// ---------------------------------------------------------------------------

/// A cluster of tool invocations of the same canonical kind.
#[derive(Clone, Debug)]
struct Cluster {
    kind: String,
    count: usize,
    paths: Vec<String>,
}

/// Group `TurnPart::ToolInvocation` parts by canonical tool kind.
///
/// Non-invocation parts (Prose, ToolResult, Reasoning) are filtered out.
fn cluster_invocations(parts: &[TurnPart]) -> Vec<Cluster> {
    let mut map: BTreeMap<String, Cluster> = BTreeMap::new();

    for part in parts {
        if let TurnPart::ToolInvocation { tool, args, .. } = part {
            let kind = canonical_tool_kind(tool);
            let entry = map.entry(kind.clone()).or_insert(Cluster {
                kind,
                count: 0,
                paths: Vec::new(),
            });
            entry.count += 1;
            if let Some(p) = extract_path_from_args(args) {
                entry.paths.push(p.to_string());
            }
        }
    }

    let mut clusters: Vec<Cluster> = map.into_values().collect();
    clusters.sort_by(|a, b| {
        std::cmp::Reverse(a.count)
            .cmp(&std::cmp::Reverse(b.count))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    clusters
}

/// Format a single cluster as `"<count> <verb>"` with an optional `" in <prefix>"` qualifier.
fn format_cluster(cluster: &Cluster) -> String {
    let verb = pluralize_verb(&cluster.kind, cluster.count);
    let mut result = format!("{} {}", cluster.count, verb);

    if cluster.count >= 2 && !cluster.paths.is_empty() {
        let path_refs: Vec<&str> = cluster.paths.iter().map(String::as_str).collect();
        let lcp = longest_common_path_prefix(&path_refs);
        if !lcp.is_empty() {
            result.push_str(&format!(" in {}", lcp));
        }
    }

    result
}

/// Assemble the Tier-2 string from sorted clusters.
///
/// - Cap at 4 clusters; trail with `, +N more` if more exist.
/// - Self-bound at 120 chars; drop trailing clusters and append U+2026 `…` if exceeded.
/// - Degenerate (zero-cluster) case not handled here — caller returns Tier-1 instead.
fn assemble_tier2(clusters: &[Cluster]) -> String {
    if clusters.is_empty() {
        return String::new();
    }

    let max_listed = 4.min(clusters.len());
    let overflow = clusters.len().saturating_sub(max_listed);

    let mut result = clusters[..max_listed]
        .iter()
        .map(format_cluster)
        .collect::<Vec<_>>()
        .join(", ");

    if overflow > 0 {
        result.push_str(&format!(", +{} more", overflow));
    }

    // 120-char self-bound with U+2026 truncation
    if result.chars().count() > 120 {
        // Drop trailing clusters until the result + ellipsis fits within 121 chars
        let mut listed_count = max_listed;
        loop {
            let mut candidate = if listed_count == 0 {
                String::new()
            } else {
                let mut s = clusters[..listed_count.min(clusters.len())]
                    .iter()
                    .map(format_cluster)
                    .collect::<Vec<_>>()
                    .join(", ");
                // Re-add overflow suffix if needed
                let remaining_overflow = clusters.len().saturating_sub(listed_count);
                if remaining_overflow > 0 {
                    s.push_str(&format!(", +{} more", remaining_overflow));
                }
                s
            };

            if candidate.is_empty() || candidate.chars().count() > 120 {
                if listed_count == 0 {
                    // Pathological case: single cluster already exceeds 120 chars.
                    // Hard-truncate to 120 chars + ellipsis.
                    let truncated: String = result.chars().take(120).collect();
                    return format!("{}\u{2026}", truncated);
                }
                listed_count -= 1;
                continue;
            }

            candidate.push('\u{2026}');
            return candidate;
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::turn::{InvocationStatus, PartId, TurnPart};
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helper constructors
    // -----------------------------------------------------------------------

    /// Create a Turn with N invocations of the same tool kind, each with a
    /// distinct file_path under a common prefix. Useful for single-cluster tests.
    fn make_turn_with_kind_and_paths(kind: &str, n: usize, paths: &[&str]) -> Turn {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for i in 0..n {
            let path = paths.get(i).copied().unwrap_or("src/unknown.rs");
            turn.push_part(|id| TurnPart::ToolInvocation {
                id,
                tool: kind.to_string(),
                args: json!({"file_path": path}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            });
        }
        turn
    }

    /// Create a Turn with N invocations of the same tool kind (no path).
    fn make_turn_with_kind(kind: &str, n: usize) -> Turn {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for _ in 0..n {
            turn.push_part(|id| TurnPart::ToolInvocation {
                id,
                tool: kind.to_string(),
                args: json!({}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            });
        }
        turn
    }

    /// Push a single invocation onto the given turn.
    fn push_invocation(
        turn: &mut Turn,
        tool: &str,
        args: serde_json::Value,
        status: InvocationStatus,
    ) {
        let tool = tool.to_string();
        turn.push_part(move |id| TurnPart::ToolInvocation {
            id,
            tool,
            args,
            status,
            started_at: 1_700_000_000_000,
            ended_at: Some(1_700_000_005_000),
        });
    }

    /// Legacy helper — kept only because the original stub tests reference it.
    fn make_turn(n_tools: usize) -> Turn {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for i in 0..(n_tools.max(1)) {
            turn.push_part(|id| TurnPart::ToolInvocation {
                id,
                tool: format!("tool_{}", i),
                args: json!({}),
                status: InvocationStatus::Success,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_005_000),
            });
        }
        turn
    }

    // -----------------------------------------------------------------------
    // Task 1.5 — Helper function unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn canonical_tool_kind_lowercases() {
        assert_eq!(canonical_tool_kind("Read"), "read");
        assert_eq!(canonical_tool_kind("BASH"), "bash");
        assert_eq!(canonical_tool_kind("My_Tool"), "my_tool");
    }

    #[test]
    fn pluralize_irregular_bash() {
        assert_eq!(pluralize_verb("bash", 1), "bash");
        assert_eq!(pluralize_verb("bash", 5), "bashes");
    }

    #[test]
    fn pluralize_default_appends_s() {
        assert_eq!(pluralize_verb("read", 1), "read");
        assert_eq!(pluralize_verb("read", 3), "reads");
    }

    #[test]
    fn extract_path_priority_file_path_wins() {
        let args = json!({"file_path": "a", "filePath": "b", "path": "c"});
        assert_eq!(extract_path_from_args(&args), Some("a"));
    }

    #[test]
    fn extract_path_falls_through_to_filePath() {
        let args = json!({"filePath": "b", "path": "c"});
        assert_eq!(extract_path_from_args(&args), Some("b"));
    }

    #[test]
    fn extract_path_falls_through_to_path() {
        let args = json!({"path": "c"});
        assert_eq!(extract_path_from_args(&args), Some("c"));
    }

    #[test]
    fn extract_path_none_when_no_keys() {
        let args = json!({"command": "ls"});
        assert_eq!(extract_path_from_args(&args), None);
    }

    #[test]
    fn extract_path_none_when_empty_string() {
        let args = json!({"file_path": ""});
        assert_eq!(extract_path_from_args(&args), None);
    }

    #[test]
    fn lcp_strips_to_last_slash_boundary() {
        let paths = &["src/auth/foo.rs", "src/util/bar.rs"];
        assert_eq!(longest_common_path_prefix(paths), "src/");
    }

    #[test]
    fn lcp_exact_dir_match() {
        let paths = &["src/auth/a.rs", "src/auth/b.rs"];
        assert_eq!(longest_common_path_prefix(paths), "src/auth/");
    }

    #[test]
    fn lcp_empty_when_disjoint() {
        let paths = &["src/foo", "tests/bar"];
        assert_eq!(longest_common_path_prefix(paths), "");
    }

    #[test]
    fn lcp_root_slash_returns_empty() {
        let paths = &["/foo", "/bar"];
        assert_eq!(longest_common_path_prefix(paths), "");
    }

    #[test]
    fn lcp_single_path_returns_empty() {
        // Defensive: single path returns empty (caller drops qualifier at count==1)
        assert_eq!(longest_common_path_prefix(&["src/a.rs"]), "");
    }

    #[test]
    fn lcp_zero_paths_returns_empty() {
        assert_eq!(longest_common_path_prefix(&[]), "");
    }

    // -----------------------------------------------------------------------
    // Preserved Tier-1 stub tests (AC1 — back-compat)
    // -----------------------------------------------------------------------

    #[test]
    fn stub_tier1_format_with_no_tools() {
        let turn = Turn::new("claude".into(), 1_700_000_000_000);
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier1, "0 tools");
        assert_eq!(label.tier2, "0 tools");
    }

    #[test]
    fn stub_tier1_pluralizes_correctly() {
        assert_eq!(compute_summary_label(&make_turn(1), None).tier1, "1 tool");
        assert_eq!(compute_summary_label(&make_turn(2), None).tier1, "2 tools");
        assert_eq!(compute_summary_label(&make_turn(5), None).tier1, "5 tools");
    }

    #[test]
    fn stub_elapsed_suffix_appears_when_provided() {
        let label = compute_summary_label(&make_turn(2), Some(12_500));
        assert_eq!(label.tier1, "2 tools, 12.5s");
    }

    #[test]
    fn stub_elapsed_suffix_omitted_when_zero_or_none() {
        assert_eq!(
            compute_summary_label(&make_turn(1), Some(0)).tier1,
            "1 tool"
        );
    }

    // -----------------------------------------------------------------------
    // AC1 — Tier-1/Tier-2 replacement tests (replaces stub_tier2_equals_tier1)
    // -----------------------------------------------------------------------

    #[test]
    fn tier2_equals_tier1_when_zero_tools() {
        let turn = Turn::new("claude".into(), 1_700_000_000_000);
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "0 tools");
    }

    #[test]
    fn tier2_diverges_from_tier1_when_invocations_present() {
        let turn = make_turn_with_kind_and_paths(
            "Read",
            3,
            &["src/auth/a.rs", "src/auth/b.rs", "src/auth/c.rs"],
        );
        let label = compute_summary_label(&turn, None);
        assert_ne!(label.tier1, label.tier2);
    }

    #[test]
    fn tier1_format_locked() {
        // Zero-tool
        let t = Turn::new("claude".into(), 1_700_000_000_000);
        assert_eq!(compute_summary_label(&t, None).tier1, "0 tools");

        // Single-tool
        let t = make_turn_with_kind("Read", 1);
        assert_eq!(compute_summary_label(&t, None).tier1, "1 tool");

        // Plural
        let t = make_turn_with_kind("Read", 3);
        assert_eq!(compute_summary_label(&t, None).tier1, "3 tools");

        // With elapsed
        let t = make_turn_with_kind("Read", 3);
        assert_eq!(
            compute_summary_label(&t, Some(7_300)).tier1,
            "3 tools, 7.3s"
        );
    }

    // -----------------------------------------------------------------------
    // AC2 — Tier-2 clusters by canonical tool kind
    // -----------------------------------------------------------------------

    #[test]
    fn tier2_canonical_kind_lowercases_pascalcase() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/auth/foo.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "read",
            json!({"file_path": "src/auth/bar.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "READ",
            json!({"file_path": "src/auth/baz.rs"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "3 reads in src/auth/");
    }

    #[test]
    fn tier2_unknown_tool_kind_passthrough_lowercased() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "mcp__filesystem__read_text_file",
            json!({}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "MyCustomTool",
            json!({}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        // Both are count=1 (alphabetical tiebreak: m* < my*), no path qualifiers
        assert_eq!(
            label.tier2,
            "1 mcp__filesystem__read_text_file, 1 mycustomtool"
        );
    }

    // -----------------------------------------------------------------------
    // AC3 — Multi-cluster format with descending-count ordering
    // -----------------------------------------------------------------------

    #[test]
    fn tier2_descending_count_ordering() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for _ in 0..3 {
            push_invocation(
                &mut turn,
                "Read",
                json!({"file_path": "src/auth/a.rs"}),
                InvocationStatus::Success,
            );
        }
        push_invocation(
            &mut turn,
            "Grep",
            json!({"pattern": "x", "path": "src/"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "3 reads in src/auth/, 1 grep");
    }

    #[test]
    fn tier2_alphabetical_tiebreak_when_counts_equal() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/foo.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Bash",
            json!({"command": "ls"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "1 bash, 1 read");
    }

    #[test]
    fn tier2_count_1_drops_path_qualifier() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/auth/foo.rs"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "1 read");
    }

    // -----------------------------------------------------------------------
    // AC4 — Path-prefix detection edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn lcp_strips_to_last_slash_boundary_test() {
        // Already covered by helper test above, but this AC-specific test
        // validates end-to-end through the labeler.
        let turn =
            make_turn_with_kind_and_paths("Read", 2, &["src/auth/foo.rs", "src/util/bar.rs"]);
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "2 reads in src/");
    }

    #[test]
    fn lcp_exact_dir_match_test() {
        let turn =
            make_turn_with_kind_and_paths("Read", 2, &["src/auth/foo.rs", "src/auth/bar.rs"]);
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "2 reads in src/auth/");
    }

    #[test]
    fn lcp_empty_when_paths_disjoint() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/foo.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "tests/bar.rs"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "2 reads");
    }

    #[test]
    fn lcp_single_root_slash_drops_qualifier() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "/foo.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "/bar.rs"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "2 reads");
    }

    #[test]
    fn path_key_probing_priority() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        // file_path wins over filePath
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "w", "filePath": "ignored"}),
            InvocationStatus::Success,
        );
        // filePath only
        push_invocation(
            &mut turn,
            "Read",
            json!({"filePath": "y"}),
            InvocationStatus::Success,
        );
        // path only
        push_invocation(
            &mut turn,
            "Read",
            json!({"path": "z"}),
            InvocationStatus::Success,
        );
        // file_path only
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "x"}),
            InvocationStatus::Success,
        );
        // Paths should be: w, x, y, z — LCP across all 4 is empty
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "4 reads");
    }

    #[test]
    fn cluster_with_no_paths_drops_qualifier() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Bash",
            json!({"command": "ls -la"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Bash",
            json!({"command": "pwd"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Bash",
            json!({"command": "echo x"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "3 bashes");
    }

    // -----------------------------------------------------------------------
    // AC5 — Pluralization
    // -----------------------------------------------------------------------

    #[test]
    fn pluralize_irregular_bash_to_bashes() {
        let turn = make_turn_with_kind("Bash", 5);
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "5 bashes");
    }

    #[test]
    fn pluralize_default_appends_s_test() {
        // Reads
        let t = make_turn_with_kind("Read", 2);
        assert_eq!(compute_summary_label(&t, None).tier2, "2 reads");
        // Greps
        let t = make_turn_with_kind("Grep", 2);
        assert_eq!(compute_summary_label(&t, None).tier2, "2 greps");
        // Globs
        let t = make_turn_with_kind("Glob", 2);
        assert_eq!(compute_summary_label(&t, None).tier2, "2 globs");
        // Edits
        let t = make_turn_with_kind("Edit", 2);
        assert_eq!(compute_summary_label(&t, None).tier2, "2 edits");
        // Writes
        let t = make_turn_with_kind("Write", 2);
        assert_eq!(compute_summary_label(&t, None).tier2, "2 writes");
        // Webfetches
        let t = make_turn_with_kind("WebFetch", 2);
        assert_eq!(compute_summary_label(&t, None).tier2, "2 webfetchs");
    }

    #[test]
    fn pluralize_unknown_kind_appends_s() {
        let turn = make_turn_with_kind("mcp__foo", 2);
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "2 mcp__foos");
    }

    // -----------------------------------------------------------------------
    // AC6 — Cluster cap at 4 with +N more overflow
    // -----------------------------------------------------------------------

    #[test]
    fn tier2_cap_4_clusters_with_plus_more_2() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for kind in &["Read", "Bash", "Grep", "Glob", "Edit", "Write"] {
            push_invocation(&mut turn, kind, json!({}), InvocationStatus::Success);
        }
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "1 bash, 1 edit, 1 glob, 1 grep, +2 more");
    }

    #[test]
    fn tier2_no_overflow_at_exactly_4() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for kind in &["Read", "Bash", "Grep", "Glob"] {
            push_invocation(&mut turn, kind, json!({}), InvocationStatus::Success);
        }
        let label = compute_summary_label(&turn, None);
        assert!(!label.tier2.contains("more"));
    }

    #[test]
    fn tier2_no_overflow_at_3_or_fewer() {
        for n in 1..=3 {
            let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
            for kind in &["Read", "Bash", "Grep"][..n] {
                push_invocation(&mut turn, kind, json!({}), InvocationStatus::Success);
            }
            let label = compute_summary_label(&turn, None);
            assert!(!label.tier2.contains("more"), "n={} should not overflow", n);
        }
    }

    // -----------------------------------------------------------------------
    // AC7 — Bounded length (120-char self-imposed cap with U+2026)
    // -----------------------------------------------------------------------

    #[test]
    fn tier2_long_string_truncates_with_ellipsis() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        let long_prefix =
            "some/very/long/path/prefix/that/is/really/deep/in/the/directory/tree/and/still/going";
        // Create multiple clusters each with count >= 2 so path qualifiers appear.
        // Each cluster with a long common prefix should push the string past 120 chars.
        for kind in &["Read", "Write", "Edit", "Grep", "Glob", "Bash"] {
            for _ in 0..2 {
                push_invocation(
                    &mut turn,
                    kind,
                    json!({"file_path": format!("{}/file.rs", long_prefix)}),
                    InvocationStatus::Success,
                );
            }
        }
        let label = compute_summary_label(&turn, None);
        let tier2 = &label.tier2;
        assert!(
            tier2.contains('\u{2026}'),
            "Expected ellipsis in truncated output: {}",
            tier2
        );
        assert!(
            tier2.chars().count() <= 121,
            "Exceeded 121 char limit: {} ({} chars)",
            tier2,
            tier2.chars().count()
        );
    }

    #[test]
    fn tier2_short_string_no_ellipsis() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/a.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/b.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Grep",
            json!({"path": "src/"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert!(!label.tier2.contains('\u{2026}'));
    }

    // -----------------------------------------------------------------------
    // AC8 — Clustering is deterministic across runs
    // -----------------------------------------------------------------------

    #[test]
    fn clustering_is_deterministic_across_100_runs() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for kind in &["Read", "Bash", "Grep", "Edit", "Read", "Bash"] {
            push_invocation(
                &mut turn,
                kind,
                json!({"file_path": format!("src/{}.rs", kind.to_lowercase())}),
                InvocationStatus::Success,
            );
        }
        let first = compute_summary_label(&turn, None).tier2;
        for _ in 0..99 {
            assert_eq!(compute_summary_label(&turn, None).tier2, first);
        }
    }

    // -----------------------------------------------------------------------
    // AC9 — Non-invocation parts are ignored
    // -----------------------------------------------------------------------

    #[test]
    fn non_invocation_parts_filtered_out() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        turn.push_part(|id| TurnPart::Prose {
            id,
            text: "hello".into(),
        });
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/a.rs"}),
            InvocationStatus::Success,
        );
        turn.push_part(|id| TurnPart::ToolResult {
            id,
            refs: PartId(1),
            output: crate::domain::models::turn::ToolOutput {
                content: "output".into(),
                is_error: false,
            },
        });
        turn.push_part(|id| TurnPart::Reasoning {
            id,
            text: "thinking...".into(),
        });
        push_invocation(
            &mut turn,
            "Bash",
            json!({"command": "ls"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier1, "2 tools");
        assert_eq!(label.tier2, "1 bash, 1 read");
    }

    // -----------------------------------------------------------------------
    // AC10 — InvocationStatus does NOT affect cluster count
    // -----------------------------------------------------------------------

    #[test]
    fn all_statuses_count_in_cluster() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for status in &[
            InvocationStatus::Success,
            InvocationStatus::Error,
            InvocationStatus::Cancelled,
            InvocationStatus::Running,
        ] {
            push_invocation(
                &mut turn,
                "Read",
                json!({"file_path": "src/auth/a.rs"}),
                status.clone(),
            );
        }
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "4 reads in src/auth/");
    }

    #[test]
    fn running_invocation_counts_in_cluster() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/a.rs"}),
            InvocationStatus::Running,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/b.rs"}),
            InvocationStatus::Running,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "2 reads in src/");
    }

    // -----------------------------------------------------------------------
    // AC11 — 8 epic-mandated cluster case fixtures
    // -----------------------------------------------------------------------

    /// #1: cluster_read_heavy_with_common_prefix
    #[test]
    fn cluster_read_heavy_with_common_prefix() {
        let turn = make_turn_with_kind_and_paths(
            "Read",
            5,
            &[
                "src/auth/login.rs",
                "src/auth/jwt.rs",
                "src/auth/session.rs",
                "src/auth/csrf.rs",
                "src/auth/oauth.rs",
            ],
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "5 reads in src/auth/");
    }

    /// #2: cluster_mixed_kinds_with_separators
    #[test]
    fn cluster_mixed_kinds_with_separators() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/foo.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/bar.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Bash",
            json!({"command": "ls -la"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Edit",
            json!({"file_path": "src/auth/foo.rs"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "2 reads in src/, 1 bash, 1 edit");
    }

    /// #3: cluster_failure_containing_does_not_filter
    #[test]
    fn cluster_failure_containing_does_not_filter() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/a.rs"}),
            InvocationStatus::Error,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/b.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/c.rs"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "3 reads in src/");
    }

    /// #4: cluster_single_tool
    #[test]
    fn cluster_single_tool() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/auth/login.rs"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "1 read");
    }

    /// #5: cluster_all_bash_no_path_qualifier
    #[test]
    fn cluster_all_bash_no_path_qualifier() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        for _ in 0..4 {
            push_invocation(
                &mut turn,
                "Bash",
                json!({"command": "ls -la"}),
                InvocationStatus::Success,
            );
        }
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "4 bashes");
    }

    /// #6: cluster_grep_only_with_path
    #[test]
    fn cluster_grep_only_with_path() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Grep",
            json!({"path": "src/auth/"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Grep",
            json!({"path": "src/auth/"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "2 greps in src/auth/");
    }

    /// #7: cluster_mixed_paths_no_common_prefix
    #[test]
    fn cluster_mixed_paths_no_common_prefix() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/foo.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "tests/bar.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "docs/baz.md"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "3 reads");
    }

    /// #8: cluster_parallel_calls_one_each
    #[test]
    fn cluster_parallel_calls_one_each() {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        push_invocation(
            &mut turn,
            "Read",
            json!({"file_path": "src/a.rs"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Bash",
            json!({"command": "ls"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Grep",
            json!({"path": "src/"}),
            InvocationStatus::Success,
        );
        push_invocation(
            &mut turn,
            "Write",
            json!({"file_path": "out.txt"}),
            InvocationStatus::Success,
        );
        let label = compute_summary_label(&turn, None);
        assert_eq!(label.tier2, "1 bash, 1 grep, 1 read, 1 write");
    }
}
