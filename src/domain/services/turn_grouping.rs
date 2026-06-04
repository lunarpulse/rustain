//! Deterministic within-session turn grouping (Story 11.6, Algorithm A+).
//!
//! [`group_turns`] is a **pure CPU fold** over a conversation's `&[Turn]`: a
//! single left-to-right pass with an [`OpenGroup`] accumulator that emits a
//! boundary on the first matching ADR-11-2 signal (R1 → R3 → R4 → R5), then
//! finalises each group with a deterministic, template-based gist.
//!
//! # Hard determinism contract (AC-11.6.1)
//!
//! No clock read (timestamps come from `Turn.started_at`, already in the data),
//! no RNG, no LLM. All sets are [`BTreeSet`] so iteration order can never leak
//! non-determinism. Two runs over the same turn list — in the same process or
//! across cold processes — return byte-identical `Vec<TurnGroup>`.
//!
//! # R1 is structural (the reconciliation that makes the LRU trim lossless)
//!
//! In rustain a tool invocation and its result are parts of the **same**
//! assistant `Turn` (`ToolResult.refs → ToolInvocation.id`, both turn-local), so
//! a tool chain never crosses a turn boundary. Keeping whole turns inside one
//! group therefore guarantees a tool_use/tool_result pair is never split — which
//! is what lets the group-granular trim in `WindowingAssembler` be "structurally
//! lossless" (CMV "no orphaned dependencies"). R1 here is the *open-turn
//! coherence guard*: while the open group's latest turn still has an unresolved
//! tool call, no boundary is emitted (the chain is extended until it resolves).

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::domain::models::MessageRole;
use crate::domain::models::turn::{Turn, TurnPart};
use crate::domain::models::turn_group::{
    BoundaryRule, GroupId, GroupSignature, GroupingConfig, RoleCounts, TurnGroup, jaccard_distance,
    jaccard_similarity,
};
use crate::domain::services::summary_labeler::extract_path_from_args;

/// Jaccard-similarity threshold for the AC-11.6.10 supersedes ("[cont.]") edge.
const SUPERSEDES_SIMILARITY: f32 = 0.7;
/// Hard cap on a gist's character length (AC-11.6.3).
const GIST_MAX_CHARS: usize = 280;

/// Group a conversation's turns into deterministic topic-clusters.
///
/// Single pass; the final open group is always flushed (end-of-session flush is
/// exempt from `min_group_turns` suppression). Returns groups in chronological
/// order; `turn_indices` index back into `turns`.
pub fn group_turns(turns: &[Turn], config: &GroupingConfig) -> Vec<TurnGroup> {
    if turns.is_empty() {
        return Vec::new();
    }

    let mut finalized: Vec<TurnGroup> = Vec::new();
    // First group: no rule started it (AC-11.6.11 → boundary_reason None).
    let mut open = OpenGroup::start(0, &turns[0], None);

    for (i, t) in turns.iter().enumerate().skip(1) {
        match decide_boundary(&open, t, config) {
            None => open.extend(i, t),
            Some(rule) => {
                if open.turn_indices.len() < config.min_group_turns {
                    // Suppress the boundary: a group must reach min_group_turns
                    // before it can split (end-of-session flush is the only
                    // exemption, handled after the loop).
                    open.extend(i, t);
                } else {
                    finalized.push(open.finalize(turns, &finalized));
                    open = OpenGroup::start(i, t, Some(rule));
                }
            }
        }
    }

    finalized.push(open.finalize(turns, &finalized));
    finalized
}

/// Evaluate the ADR-11-2 boundary signals in strict precedence order. Returns
/// `Some(rule)` for the first matching *break* signal, or `None` to extend.
///
/// R1 (tool-chain integrity) is an *anti-break* guard: when the open group's
/// latest turn still has an unresolved tool call, return `None` (extend) before
/// any other rule can fire.
fn decide_boundary(open: &OpenGroup, t: &Turn, config: &GroupingConfig) -> Option<BoundaryRule> {
    // R1 — open-turn coherence guard. Highest precedence; suppresses all breaks.
    if open.last_turn_unresolved {
        return None;
    }

    // R3 — time gap (millis; Turn.started_at is unix millis).
    let gap_ms = (config.t_gap_minutes as i64) * 60_000;
    if (t.started_at - open.last_touched_at).max(0) > gap_ms {
        return Some(BoundaryRule::R3TimeGap);
    }

    // R4 — file-set Jaccard drift (only when the incoming turn touches files).
    let files_t = files(t);
    if !files_t.is_empty() && jaccard_distance(&files_t, &open.file_set) > config.jaccard_threshold
    {
        return Some(BoundaryRule::R4FileDrift);
    }

    // R5 — tool-chain break: every tool in the group resolved AND the incoming
    // turn's tool names are disjoint from the group's (a clean tool-set switch).
    if open.group_unresolved == 0 {
        let tools_t = tool_names(t);
        if !tools_t.is_empty() && tools_t.is_disjoint(&open.tool_names) {
            return Some(BoundaryRule::R5ToolBreak);
        }
    }

    None
}

/// File paths touched by a turn's tool invocations (R4 file-set drift).
fn files(turn: &Turn) -> BTreeSet<PathBuf> {
    let mut set = BTreeSet::new();
    for part in &turn.parts {
        if let TurnPart::ToolInvocation { args, .. } = part {
            if let Some(p) = extract_path_from_args(args) {
                set.insert(PathBuf::from(p));
            }
        }
    }
    set
}

/// Tool names invoked in a turn (R5 tool-chain break).
fn tool_names(turn: &Turn) -> BTreeSet<String> {
    turn.parts
        .iter()
        .filter_map(|p| match p {
            TurnPart::ToolInvocation { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .collect()
}

/// Count tool invocations in a turn that have no matching intra-turn result.
/// (`ToolResult.refs → ToolInvocation.id`, both turn-local.)
fn unresolved_in_turn(turn: &Turn) -> usize {
    let resolved: BTreeSet<u64> = turn
        .parts
        .iter()
        .filter_map(|p| match p {
            TurnPart::ToolResult { refs, .. } => Some(refs.0),
            _ => None,
        })
        .collect();
    turn.parts
        .iter()
        .filter(|p| match p {
            TurnPart::ToolInvocation { id, .. } => !resolved.contains(&id.0),
            _ => false,
        })
        .count()
}

/// First sentence of `text` (ASCII-naive split on the first `. ` or `\n`).
/// The gist is a model hint, not user prose, so CJK / code-block nuance is the
/// deferred Story 11.7 LLM-gist upgrade (ADR-11-2).
fn first_sentence(text: &str) -> &str {
    let mut end = text.len();
    if let Some(idx) = text.find(". ") {
        end = end.min(idx);
    }
    if let Some(idx) = text.find('\n') {
        end = end.min(idx);
    }
    text[..end].trim()
}

/// Concatenated prose text of a turn (all `Prose` parts, space-joined).
fn turn_prose(turn: &Turn) -> String {
    let mut out = String::new();
    for part in &turn.parts {
        if let TurnPart::Prose { text, .. } = part {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
        }
    }
    out
}

/// Truncate to `GIST_MAX_CHARS` on a char boundary, appending `…` if cut.
fn cap_gist(s: String) -> String {
    if s.chars().count() <= GIST_MAX_CHARS {
        return s;
    }
    let truncated: String = s.chars().take(GIST_MAX_CHARS - 1).collect();
    format!("{truncated}\u{2026}")
}

/// Mutable accumulator for the group currently being built.
struct OpenGroup {
    turn_indices: Vec<usize>,
    file_set: BTreeSet<PathBuf>,
    tool_names: BTreeSet<String>,
    role_counts: RoleCounts,
    first_touched_at: i64,
    last_touched_at: i64,
    boundary_reason: Option<BoundaryRule>,
    /// Unresolved tool count across the whole group (R5 precondition).
    group_unresolved: usize,
    /// Whether the most-recently-added turn left a tool unresolved (R1 guard).
    last_turn_unresolved: bool,
}

impl OpenGroup {
    fn start(idx: usize, t: &Turn, boundary_reason: Option<BoundaryRule>) -> Self {
        let mut g = Self {
            turn_indices: Vec::new(),
            file_set: BTreeSet::new(),
            tool_names: BTreeSet::new(),
            role_counts: RoleCounts::default(),
            first_touched_at: t.started_at,
            last_touched_at: t.started_at,
            boundary_reason,
            group_unresolved: 0,
            last_turn_unresolved: false,
        };
        g.absorb(idx, t);
        g
    }

    fn extend(&mut self, idx: usize, t: &Turn) {
        self.absorb(idx, t);
    }

    /// Fold a turn's signature into the open group.
    fn absorb(&mut self, idx: usize, t: &Turn) {
        self.turn_indices.push(idx);
        self.last_touched_at = t.started_at;
        self.file_set.extend(files(t));
        self.tool_names.extend(tool_names(t));
        match t.role {
            MessageRole::User => self.role_counts.user += 1,
            MessageRole::Assistant => self.role_counts.assistant += 1,
            MessageRole::System => self.role_counts.system += 1,
        }
        let unresolved = unresolved_in_turn(t);
        self.group_unresolved += unresolved;
        self.last_turn_unresolved = unresolved > 0;
    }

    /// Compute the immutable [`TurnGroup`]: id, signature, bi-temporal fields,
    /// supersedes edge, and deterministic gist.
    fn finalize(self, turns: &[Turn], prior: &[TurnGroup]) -> TurnGroup {
        let first_idx = self.turn_indices[0];
        let id = GroupId::derive(&turns[first_idx].id.0, first_idx);
        let last_span = *self.turn_indices.last().unwrap();

        let signature = GroupSignature {
            file_set: self.file_set,
            tool_names: self.tool_names,
            turn_span: (first_idx, last_span),
            role_counts: self.role_counts,
        };

        // AC-11.6.10 — supersedes edge: the most-recent prior group whose
        // file-set has Jaccard-similarity ≥ 0.7 with this one (highest
        // similarity wins; ties break to the latest prior for determinism).
        let supersedes = prior
            .iter()
            .filter(|p| {
                jaccard_similarity(&p.signature.file_set, &signature.file_set)
                    >= SUPERSEDES_SIMILARITY
            })
            .max_by(|a, b| {
                let sa = jaccard_similarity(&a.signature.file_set, &signature.file_set);
                let sb = jaccard_similarity(&b.signature.file_set, &signature.file_set);
                sa.partial_cmp(&sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.signature.turn_span.0.cmp(&b.signature.turn_span.0))
            })
            .map(|p| p.id);

        let gist = build_gist(turns, &self.turn_indices, &signature, supersedes);

        TurnGroup {
            id,
            turn_indices: self.turn_indices,
            gist,
            signature,
            first_touched_at: self.first_touched_at,
            last_touched_at: self.last_touched_at,
            supersedes,
            boundary_reason: self.boundary_reason,
        }
    }
}

/// Deterministic, template-based gist (NO LLM — ADR-11-2 non-goal #2):
/// `[first user sentence] [last assistant sentence] (Nt, Mf)`, with a
/// `[cont. group #N] ` prefix when this group supersedes an earlier one. Capped
/// at ≤3 sentences (structurally — at most two are used) and ≤280 chars.
fn build_gist(
    turns: &[Turn],
    turn_indices: &[usize],
    signature: &GroupSignature,
    supersedes: Option<GroupId>,
) -> String {
    let first_user = turn_indices
        .iter()
        .map(|&i| &turns[i])
        .find(|t| t.role == MessageRole::User)
        .map(|t| first_sentence(&turn_prose(t)).to_string())
        .filter(|s| !s.is_empty());

    let last_assistant = turn_indices
        .iter()
        .rev()
        .map(|&i| &turns[i])
        .find(|t| t.role == MessageRole::Assistant)
        .map(|t| first_sentence(&turn_prose(t)).to_string())
        .filter(|s| !s.is_empty());

    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = first_user {
        parts.push(s);
    }
    if let Some(s) = last_assistant {
        parts.push(s);
    }
    let n_turns = turn_indices.len();
    let n_files = signature.file_set.len();
    let counts = format!("({n_turns}t, {n_files}f)");

    let body = if parts.is_empty() {
        counts
    } else {
        format!("{} {}", parts.join(" "), counts)
    };

    let gist = match supersedes {
        Some(prev) => format!("[cont. group #{}] {body}", prev.0),
        None => body,
    };
    cap_gist(gist)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::turn::{InvocationStatus, PartId, Turn, TurnPart};
    use serde_json::json;

    // ── fixture builders ────────────────────────────────────────────────

    /// An assistant turn with prose + the given (tool, path) invocations
    /// (all resolved with a result), at `started_at` millis.
    fn assistant_turn(started_at: i64, prose: &str, tools: &[(&str, &str)]) -> Turn {
        let mut t = Turn::new("m".into(), started_at);
        if !prose.is_empty() {
            t.push_part(|id| TurnPart::Prose {
                id,
                text: prose.to_string(),
            });
        }
        for (tool, path) in tools {
            let inv = t.push_part(|id| TurnPart::ToolInvocation {
                id,
                tool: tool.to_string(),
                args: json!({ "file_path": path }),
                status: InvocationStatus::Success,
                started_at,
                ended_at: Some(started_at + 1),
            });
            t.push_part(|id| TurnPart::ToolResult {
                id,
                refs: inv,
                output: crate::domain::models::turn::ToolOutput {
                    content: "ok".into(),
                    is_error: false,
                },
            });
        }
        t
    }

    /// An assistant turn with one UNRESOLVED tool invocation (no result part).
    fn assistant_turn_pending(started_at: i64, tool: &str, path: &str) -> Turn {
        let mut t = Turn::new("m".into(), started_at);
        t.push_part(|id| TurnPart::ToolInvocation {
            id,
            tool: tool.to_string(),
            args: json!({ "file_path": path }),
            status: InvocationStatus::Running,
            started_at,
            ended_at: None,
        });
        t
    }

    fn user_turn(started_at: i64, prose: &str) -> Turn {
        Turn::user(prose.to_string(), started_at)
    }

    const MIN: i64 = 60_000;

    // ── AC-11.6.1 determinism ───────────────────────────────────────────

    fn sample_conversation() -> Vec<Turn> {
        vec![
            assistant_turn(0, "Work on auth", &[("Read", "src/auth/a.rs")]),
            assistant_turn(1000, "More auth", &[("Edit", "src/auth/b.rs")]),
            // big time gap + different file area → boundary
            assistant_turn(60 * MIN, "Now the parser", &[("Read", "src/parser/p.rs")]),
            assistant_turn(
                60 * MIN + 1000,
                "parser cont",
                &[("Edit", "src/parser/q.rs")],
            ),
        ]
    }

    #[test]
    fn deterministic_twice_in_process() {
        let turns = sample_conversation();
        let cfg = GroupingConfig::default();
        let a = group_turns(&turns, &cfg);
        let b = group_turns(&turns, &cfg);
        assert_eq!(a, b);
        assert!(a.len() >= 2, "expected a split, got {}", a.len());
    }

    #[test]
    fn deterministic_across_cold_process_via_serialized_fixture() {
        let turns = sample_conversation();
        let cfg = GroupingConfig::default();
        let first = group_turns(&turns, &cfg);

        // Simulate a cold process: serialize the input, deserialize, re-run.
        let json = serde_json::to_string(&turns).unwrap();
        let reloaded: Vec<Turn> = serde_json::from_str(&json).unwrap();
        let second = group_turns(&reloaded, &cfg);

        assert_eq!(
            first, second,
            "grouping must be byte-identical across processes"
        );
    }

    // ── AC-11.6.8 one-rule-per-case boundary matrix ─────────────────────

    #[test]
    fn r3_time_gap_splits() {
        let turns = vec![
            assistant_turn(0, "a", &[("Read", "src/a.rs")]),
            assistant_turn(1000, "b", &[("Read", "src/a.rs")]),
            // > 15 min gap, same files → only R3 can fire
            assistant_turn(20 * MIN, "c", &[("Read", "src/a.rs")]),
            assistant_turn(20 * MIN + 1000, "d", &[("Read", "src/a.rs")]),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].boundary_reason, Some(BoundaryRule::R3TimeGap));
    }

    #[test]
    fn r4_file_drift_splits() {
        // No time gap, all resolved, but the file-set drifts past 0.4 distance.
        let turns = vec![
            assistant_turn(0, "a", &[("Read", "src/auth/a.rs")]),
            assistant_turn(1000, "b", &[("Read", "src/auth/b.rs")]),
            assistant_turn(2000, "c", &[("Read", "src/totally/different.rs")]),
            assistant_turn(3000, "d", &[("Read", "src/totally/other.rs")]),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].boundary_reason, Some(BoundaryRule::R4FileDrift));
    }

    #[test]
    fn r5_tool_break_splits() {
        // No gap, no files (so R4 cannot fire), all resolved; disjoint tool set.
        let mk = |ts: i64, tool: &str| {
            let mut t = Turn::new("m".into(), ts);
            let inv = t.push_part(|id| TurnPart::ToolInvocation {
                id,
                tool: tool.to_string(),
                args: json!({ "command": "x" }),
                status: InvocationStatus::Success,
                started_at: ts,
                ended_at: Some(ts + 1),
            });
            t.push_part(|id| TurnPart::ToolResult {
                id,
                refs: inv,
                output: crate::domain::models::turn::ToolOutput {
                    content: "ok".into(),
                    is_error: false,
                },
            });
            t
        };
        let turns = vec![
            mk(0, "Bash"),
            mk(1000, "Bash"),
            mk(2000, "WebFetch"),
            mk(3000, "WebFetch"),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].boundary_reason, Some(BoundaryRule::R5ToolBreak));
    }

    #[test]
    fn r1_unresolved_tool_force_extends_over_time_gap() {
        // Open group's last turn has an unresolved tool → R1 suppresses the R3
        // time-gap boundary that would otherwise fire.
        let turns = vec![
            assistant_turn(0, "a", &[("Read", "src/a.rs")]),
            assistant_turn_pending(1000, "Bash", "src/a.rs"),
            // huge gap — R3 would fire, but R1 guard extends instead
            assistant_turn(60 * MIN, "c", &[("Read", "src/a.rs")]),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(groups.len(), 1, "R1 must keep the chain in one group");
    }

    #[test]
    fn min_group_turns_suppresses_early_boundary() {
        // A boundary signal on turn index 1 (group has only 1 turn) is suppressed.
        let turns = vec![
            assistant_turn(0, "a", &[("Read", "src/auth/a.rs")]),
            // would-be R3 break at the 2nd turn, but min_group_turns=2 suppresses
            assistant_turn(60 * MIN, "b", &[("Read", "src/auth/a.rs")]),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(
            groups.len(),
            1,
            "boundary before min_group_turns is suppressed"
        );
    }

    #[test]
    fn end_of_session_flush_emits_short_final_group() {
        // group 1 reaches min size, then a single trailing turn opens group 2 via
        // R3; group 2 has only 1 turn but the end-of-session flush still emits it.
        let turns = vec![
            assistant_turn(0, "a", &[("Read", "src/a.rs")]),
            assistant_turn(1000, "b", &[("Read", "src/a.rs")]),
            assistant_turn(60 * MIN, "c", &[("Read", "src/a.rs")]),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].turn_indices, vec![2]);
    }

    // ── AC-11.6.11 bi-temporal + boundary_reason ────────────────────────

    #[test]
    fn bitemporal_fields_and_first_group_reason_none() {
        let turns = sample_conversation();
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(groups[0].boundary_reason, None, "first group has no rule");
        let g0 = &groups[0];
        assert_eq!(g0.first_touched_at, turns[g0.turn_indices[0]].started_at);
        assert_eq!(
            g0.last_touched_at,
            turns[*g0.turn_indices.last().unwrap()].started_at
        );
    }

    // ── AC-11.6.10 supersedes ───────────────────────────────────────────

    #[test]
    fn supersedes_edge_set_on_high_file_overlap() {
        // Two groups separated by a time gap but with ≥0.7 file overlap → the
        // second supersedes the first and prefixes its gist with [cont. group #N].
        let turns = vec![
            assistant_turn(
                0,
                "auth",
                &[("Read", "src/auth/a.rs"), ("Read", "src/auth/b.rs")],
            ),
            assistant_turn(1000, "auth2", &[("Read", "src/auth/a.rs")]),
            // gap → new group, same files (similarity 1.0 with group 0's set)
            assistant_turn(
                60 * MIN,
                "auth again",
                &[("Read", "src/auth/a.rs"), ("Read", "src/auth/b.rs")],
            ),
            assistant_turn(60 * MIN + 1000, "more", &[("Read", "src/auth/a.rs")]),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].supersedes, Some(groups[0].id));
        assert!(
            groups[1]
                .gist
                .starts_with(&format!("[cont. group #{}]", groups[0].id.0)),
            "gist: {}",
            groups[1].gist
        );
    }

    // ── AC-11.6.3 gist shape ────────────────────────────────────────────

    #[test]
    fn gist_is_bounded_and_carries_counts() {
        let turns = vec![
            assistant_turn(0, "Implement the login flow", &[("Read", "src/auth/a.rs")]),
            assistant_turn(1000, "Wire it up", &[("Edit", "src/auth/b.rs")]),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert!(g.gist.chars().count() <= GIST_MAX_CHARS);
        assert!(g.gist.contains("(2t, 2f)"), "gist: {}", g.gist);
    }

    #[test]
    fn gist_uses_user_and_assistant_sentences_when_present() {
        let turns = vec![
            user_turn(0, "How do I add auth? Lots of detail follows."),
            assistant_turn(
                1000,
                "Here is the plan. Step two.",
                &[("Read", "src/auth/a.rs")],
            ),
        ];
        let groups = group_turns(&turns, &GroupingConfig::default());
        let g = &groups[0];
        assert!(g.gist.starts_with("How do I add auth?"), "gist: {}", g.gist);
        assert!(g.gist.contains("Here is the plan"), "gist: {}", g.gist);
    }

    #[test]
    fn empty_turns_yields_no_groups() {
        assert!(group_turns(&[], &GroupingConfig::default()).is_empty());
    }
}
