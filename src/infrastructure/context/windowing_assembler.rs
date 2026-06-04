//! [`WindowingAssembler`] — the within-session grouped windowing
//! [`ContextAssemblerPort`] (Story 11.6, Algorithm A+, impl #3).
//!
//! Sibling to [`StaticPassthroughAssembler`](super::StaticPassthroughAssembler):
//! a **stateless, sync, infallible** Message-tier assembler. It groups the
//! conversation's turns ([`group_turns`]), keeps the active tail group(s)
//! verbatim, replaces each cold group with a one-line gist `system` message, and
//! trims cold gists to fit the `budget` using a relevance-weighted score (S3).
//!
//! # Two-Ports boundary (the one rule that cannot break)
//!
//! This is the **Message tier** — it *places/shapes* turns into the wire payload.
//! It does NOT select or rank memory *content* (that is `ContextPort`, the
//! Content tier). No I/O, no clock, no RNG (ADR-11-2). `now` for recency is the
//! last turn's `started_at`, keeping the fold clock-free and deterministic.
//!
//! # Why active turns are materialised via a message-slice
//!
//! Grouping is over `conversation.turns` (assistant turns; user prompts live in
//! the `conversation.messages` mirror — `build_api_messages` reads the mirror,
//! not `turns`). Because grouping is a left-to-right fold, the active group(s)
//! are always **contiguous at the tail**, so the wire is
//! `[cold gists…] ++ build_api_messages(active message-slice)`. The active slice
//! starts at the user prompt that initiates the first active turn, so
//! `build_api_messages` starts with an empty tool-result buffer and pairs every
//! tool_use with its tool_result inside the active block — no orphans regardless
//! of budget (AC-11.6.4). A dropped cold turn carries away *both* its tool_use
//! and tool_result (they are parts of one turn), replaced by the gist.

use crate::domain::models::{
    AssembleDiagnostics, AssembledContext, AssemblyBudget, ContextSource, Conversation, GroupId,
    GroupingConfig, Message, MessageRole, TurnGroup, estimate_tokens, jaccard_similarity,
};
use crate::domain::ports::ContextAssemblerPort;
use crate::domain::services::message_builder::build_api_messages;
use crate::domain::services::turn_grouping::group_turns;

/// JTBD-log gate: only emit the structured token-saving log for sessions large
/// enough to be meaningful (AC-11.6.7).
const JTBD_MIN_TURNS: usize = 20;

/// The within-session grouped windowing assembler. Holds only its internal
/// (non-user-exposed) [`GroupingConfig`]; no constructor takes user config.
#[derive(Debug, Clone, Default)]
pub struct WindowingAssembler {
    config: GroupingConfig,
}

impl ContextAssemblerPort for WindowingAssembler {
    fn assemble(&self, conversation: &Conversation, budget: AssemblyBudget) -> AssembledContext {
        let turns = &conversation.turns;
        // Passthrough baseline — the apples-to-apples comparison for token
        // savings and the degenerate-case fallback.
        let passthrough = build_api_messages(conversation);
        let passthrough_tokens = wire_tokens(&passthrough);

        let groups = group_turns(turns, &self.config);

        // Degenerate: 0 or 1 group → nothing to summarise; behave like
        // passthrough but still report group_count / active_group_id.
        if groups.len() < 2 {
            return AssembledContext {
                messages: passthrough,
                diagnostics: diagnostics(
                    groups.last().map(|g| g.id).unwrap_or(GroupId(0)),
                    groups.len(),
                    Vec::new(),
                    passthrough_tokens,
                    passthrough_tokens,
                    false,
                ),
            };
        }

        let n = turns.len();
        let k = self.config.active_window_k.max(1);
        let active_threshold = n.saturating_sub(k);

        // A group is active iff its latest turn-index is within the last K turns
        // (AC-11.6.2). Groups are contiguous, so the active set is the tail.
        let is_active = |g: &TurnGroup| {
            g.turn_indices
                .iter()
                .max()
                .copied()
                .map(|mx| mx >= active_threshold)
                .unwrap_or(false)
        };
        let cold_groups: Vec<&TurnGroup> = groups.iter().filter(|g| !is_active(g)).collect();
        let active_groups: Vec<&TurnGroup> = groups.iter().filter(|g| is_active(g)).collect();

        // The anchor for relevance overlap = the active group containing the
        // last turn (the chronologically last group).
        let active_anchor = groups.last().expect("groups.len() >= 2");

        // First active turn index (active set is contiguous at the tail).
        let first_active_turn_idx = active_groups
            .iter()
            .flat_map(|g| g.turn_indices.iter().copied())
            .min()
            .expect("active set is non-empty");

        // Materialise the active block by slicing the message mirror at the user
        // prompt that initiates the first active turn. Falls back to passthrough
        // if the anchor message cannot be located (defensive — never panic).
        let active_wire = match active_message_slice(conversation, turns, first_active_turn_idx) {
            Some(active_msgs) => {
                let sub = Conversation {
                    messages: active_msgs,
                    ..Default::default()
                };
                build_api_messages(&sub)
            }
            None => {
                return AssembledContext {
                    messages: passthrough,
                    diagnostics: diagnostics(
                        GroupId(0),
                        0,
                        Vec::new(),
                        passthrough_tokens,
                        passthrough_tokens,
                        false,
                    ),
                };
            }
        };
        let active_tokens = wire_tokens(&active_wire);

        // Degenerate: active block alone exceeds budget — we still emit it
        // (active groups are never trimmed per AC-11.6.4), but warn.
        if active_tokens > budget.max_tokens {
            tracing::warn!(
                "active block ({} tokens) exceeds budget ({} tokens); emitting anyway",
                active_tokens,
                budget.max_tokens
            );
        }

        // ── S3 relevance-weighted trim (AC-11.6.4 / 9a / 9b) ────────────────
        let now_ms = turns[n - 1].started_at;
        let gist_tokens = |g: &TurnGroup| estimate_tokens(&gist_content(g));

        // Trim order: ascending priority — drop the lowest score first. Ties
        // break by (last_touched_at, id) for a total order (clock-free).
        let mut by_priority: Vec<&TurnGroup> = cold_groups.clone();
        by_priority.sort_by(|a, b| {
            let sa = s3_score(a, active_anchor, now_ms);
            let sb = s3_score(b, active_anchor, now_ms);
            sa.partial_cmp(&sb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.last_touched_at.cmp(&b.last_touched_at))
                .then(a.id.0.cmp(&b.id.0))
        });

        let mut dropped: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut total = active_tokens + cold_groups.iter().map(|g| gist_tokens(g)).sum::<usize>();
        for g in &by_priority {
            if total <= budget.max_tokens {
                break;
            }
            total -= gist_tokens(g);
            dropped.insert(g.id.0);
        }
        let truncated = !dropped.is_empty();

        // ── Emit: kept cold gists (chronological) ++ active wire ────────────
        let mut messages: Vec<Message> = Vec::with_capacity(cold_groups.len() + active_wire.len());
        let mut per_source_tokens: Vec<(ContextSource, usize)> = Vec::new();
        let mut kept_gist_tokens = 0usize;
        for g in &cold_groups {
            if dropped.contains(&g.id.0) {
                continue;
            }
            let content = gist_content(g);
            let toks = estimate_tokens(&content);
            kept_gist_tokens += toks;
            per_source_tokens.push((ContextSource::Group(g.id.0), toks));
            messages.push(gist_message(content));
        }
        messages.extend(active_wire);

        let bundle_tokens = active_tokens + kept_gist_tokens;

        // ── JTBD structured log (AC-11.6.7) ─────────────────────────────────
        let saved = passthrough_tokens as i64 - bundle_tokens as i64;
        let pct = saved_pct(saved, passthrough_tokens);
        if n >= JTBD_MIN_TURNS && groups.len() >= 2 {
            let sid = conversation
                .session_id
                .as_deref()
                .unwrap_or(&conversation.id);
            tracing::info!(
                session_id = %sid,
                group_count = groups.len(),
                tokens_saved = saved,
                tokens_saved_pct = pct,
                "windowing assembled"
            );
        }

        AssembledContext {
            messages,
            diagnostics: diagnostics(
                active_anchor.id,
                groups.len(),
                per_source_tokens,
                passthrough_tokens,
                bundle_tokens,
                truncated,
            ),
        }
    }
}

/// AC-11.6.9a/9b score: `0.5·recency + 0.5·overlap`. `recency` decays with age
/// (1h half-ish curve); `overlap` is the file-set Jaccard with the active
/// anchor. When no cold group overlaps the anchor, `overlap == 0` for all and
/// the order degrades to pure recency (LRU) — 9b.
fn s3_score(g: &TurnGroup, anchor: &TurnGroup, now_ms: i64) -> f32 {
    let age_secs = ((now_ms - g.last_touched_at).max(0) as f64) / 1000.0;
    let recency = 1.0 / (1.0 + age_secs / 3600.0);
    let overlap = jaccard_similarity(&g.signature.file_set, &anchor.signature.file_set) as f64;
    (0.5 * recency + 0.5 * overlap) as f32
}

/// The cold-group gist body: `[group {id}: {gist}]` (AC-11.6.3).
fn gist_content(g: &TurnGroup) -> String {
    format!("[group {}: {}]", g.id.0, g.gist)
}

/// A synthetic `system` message carrying a cold-group gist.
fn gist_message(content: String) -> Message {
    Message {
        role: MessageRole::System,
        content,
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
    }
}

/// Slice `conversation.messages` to the active block: everything from the user
/// prompt that initiates `first_active_turn_idx` to the end (which includes the
/// trailing live prompt). Returns `None` if the anchor assistant message cannot
/// be located in the mirror.
fn active_message_slice(
    conversation: &Conversation,
    turns: &[crate::domain::models::Turn],
    first_active_turn_idx: usize,
) -> Option<Vec<crate::domain::models::ChatMessage>> {
    use crate::domain::models::ChatMessage;
    let anchor_id = &turns[first_active_turn_idx].id.0;
    let pos = conversation
        .messages
        .iter()
        .position(|m: &ChatMessage| m.role == MessageRole::Assistant && &m.id == anchor_id)?;
    // Back up over the immediately-preceding User/System prompt(s) so the slice
    // starts at the exchange's prompt, not mid-assistant.
    let mut start = pos;
    while start > 0 && conversation.messages[start - 1].role != MessageRole::Assistant {
        start -= 1;
    }
    Some(conversation.messages[start..].to_vec())
}

/// Estimated tokens of a single wire message (content + tool payloads +
/// prefix/reasoning). Used identically for passthrough, active, and gist counts
/// so `tokens_saved_*` is apples-to-apples (AC-11.6.6).
fn message_tokens(m: &Message) -> usize {
    let mut n = estimate_tokens(&m.content);
    if let Some(p) = &m.context_prefix {
        n += estimate_tokens(p);
    }
    if let Some(r) = &m.reasoning_content {
        n += estimate_tokens(r);
    }
    for tr in &m.tool_results {
        n += estimate_tokens(&tr.content);
    }
    for tu in &m.tool_uses {
        n += estimate_tokens(&tu.name) + estimate_tokens(&tu.input.to_string());
    }
    n
}

fn wire_tokens(messages: &[Message]) -> usize {
    messages.iter().map(message_tokens).sum()
}

/// `saved / passthrough * 100`, 2-dp rounded, NaN-safe (`0.0` when passthrough
/// is `0`).
fn saved_pct(saved: i64, passthrough_tokens: usize) -> f32 {
    if passthrough_tokens == 0 {
        return 0.0;
    }
    let pct = (saved as f32 / passthrough_tokens as f32) * 100.0;
    if pct.is_nan() {
        0.0
    } else {
        (pct * 100.0).round() / 100.0
    }
}

/// Assemble the diagnostics struct (the 4 new Story-11.6 fields + reused ones).
fn diagnostics(
    active_group_id: GroupId,
    group_count: usize,
    per_source_tokens: Vec<(ContextSource, usize)>,
    passthrough_tokens: usize,
    bundle_tokens: usize,
    truncated: bool,
) -> AssembleDiagnostics {
    let saved = passthrough_tokens as i64 - bundle_tokens as i64;
    let total_tokens = bundle_tokens;
    AssembleDiagnostics {
        per_source_tokens,
        total_tokens,
        truncated,
        deduped_count: 0,
        active_group_id,
        group_count,
        tokens_saved_vs_passthrough: saved,
        tokens_saved_pct: saved_pct(saved, passthrough_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::turn::{InvocationStatus, Turn, TurnPart};
    use crate::domain::models::{
        ChatMessage, ToolCallInfo, ToolResultInfo, generate_conversation_id,
    };
    use serde_json::json;

    const MIN: i64 = 60_000;

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

    fn user_chat(content: &str) -> ChatMessage {
        ChatMessage {
            synthetic: false,
            id: generate_conversation_id(),
            role: MessageRole::User,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        }
    }

    /// Build an assistant ChatMessage mirroring a turn (id == turn id), with
    /// tool_calls derived from the turn's invocations (each resolved).
    fn assistant_chat(t: &Turn) -> ChatMessage {
        let mut content = String::new();
        let mut tool_calls = vec![];
        for part in &t.parts {
            match part {
                TurnPart::Prose { text, .. } => content.push_str(text),
                TurnPart::ToolInvocation { id, tool, args, .. } => {
                    tool_calls.push(ToolCallInfo {
                        id: format!("tc_{}_{}", t.id.0, id.0),
                        name: tool.clone(),
                        input: args.clone(),
                        result: Some(ToolResultInfo {
                            content: "ok".into(),
                            is_error: false,
                        }),
                        started_at_ms: Some(0),
                        completed_at_ms: Some(1),
                        status: Some("✓ Success".into()),
                    });
                }
                _ => {}
            }
        }
        ChatMessage {
            synthetic: false,
            id: t.id.0.clone(),
            role: MessageRole::Assistant,
            content,
            content_blocks: vec![],
            tool_calls,
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        }
    }

    /// Conversation whose mirror interleaves a user prompt before each assistant
    /// turn, plus a trailing live user prompt — matching the live event loop.
    fn conv(assistant_turns: Vec<Turn>) -> Conversation {
        let mut messages = vec![];
        for (i, t) in assistant_turns.iter().enumerate() {
            messages.push(user_chat(&format!("prompt {i}")));
            messages.push(assistant_chat(t));
        }
        messages.push(user_chat("current prompt"));
        Conversation {
            messages,
            turns: assistant_turns,
            ..Default::default()
        }
    }

    /// Two clearly-separable topics: an auth block then a parser block split by
    /// a time gap (≥2 groups guaranteed).
    fn two_topic_conv() -> Conversation {
        conv(vec![
            assistant_turn(0, "auth a", &[("Read", "src/auth/a.rs")]),
            assistant_turn(1000, "auth b", &[("Edit", "src/auth/b.rs")]),
            assistant_turn(60 * MIN, "parser c", &[("Read", "src/parser/c.rs")]),
            assistant_turn(60 * MIN + 1000, "parser d", &[("Edit", "src/parser/d.rs")]),
        ])
    }

    fn big() -> AssemblyBudget {
        AssemblyBudget {
            max_tokens: usize::MAX,
        }
    }

    // AC-11.6.2 — active group is the one containing the last turn.
    #[test]
    fn active_group_is_the_last_group() {
        let c = two_topic_conv();
        let asm = WindowingAssembler::default();
        let groups = group_turns(&c.turns, &GroupingConfig::default());
        let out = asm.assemble(&c, big());
        assert_eq!(out.diagnostics.active_group_id, groups.last().unwrap().id);
        assert_eq!(out.diagnostics.group_count, groups.len());
    }

    // AC-11.6.3 — cold groups become exactly one [group {id}: {gist}] system msg.
    #[test]
    fn cold_groups_become_one_gist_system_message_each() {
        let c = two_topic_conv();
        let asm = WindowingAssembler::default();
        let out = asm.assemble(&c, big());
        let gists: Vec<&Message> = out
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .collect();
        // 2 groups, last is active → exactly 1 cold gist.
        assert_eq!(
            gists.len(),
            1,
            "got: {:?}",
            gists.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
        assert!(gists[0].content.contains(": "));
        // Active turns appear verbatim (the parser block's content is present).
        assert!(out.messages.iter().any(|m| m.content.contains("parser")));
        // The cold (auth) assistant prose is NOT emitted verbatim — only its gist.
        let verbatim_auth = out
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.content.contains("auth"));
        assert!(!verbatim_auth, "cold turns must not appear verbatim");
    }

    // AC-11.6.2 — with K=4, both tail groups are active when the last turn
    // falls within the most recent 4 turns (spanning 2 groups).
    #[test]
    fn active_window_k4_marks_multiple_tail_groups_active() {
        // 3 groups: auth (old), parser (mid), ui (active / tail)
        let c = conv(vec![
            assistant_turn(0, "auth a", &[("Read", "src/auth/a.rs")]),
            assistant_turn(1000, "auth b", &[("Read", "src/auth/b.rs")]),
            // gap → group 2
            assistant_turn(60 * MIN, "parser c", &[("Read", "src/parser/c.rs")]),
            assistant_turn(60 * MIN + 1000, "parser d", &[("Read", "src/parser/d.rs")]),
            // gap → group 3
            assistant_turn(120 * MIN, "ui e", &[("Read", "src/ui/e.rs")]),
            assistant_turn(120 * MIN + 1000, "ui f", &[("Read", "src/ui/f.rs")]),
        ]);
        let asm = WindowingAssembler {
            config: GroupingConfig {
                active_window_k: 4,
                ..GroupingConfig::default()
            },
        };
        let groups = group_turns(&c.turns, &asm.config);
        assert!(groups.len() >= 3, "need ≥3 groups, got {}", groups.len());
        let out = asm.assemble(&c, big());
        // With K=4, the last 4 turns (parser + ui groups) are active.
        // Only the oldest (auth) should be cold → 1 gist.
        let gists: Vec<&Message> = out
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .collect();
        assert_eq!(
            gists.len(),
            1,
            "with K=4 only the oldest group should be cold; got: {:?}",
            gists.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
        // Active turns from both parser and ui blocks appear verbatim.
        assert!(out.messages.iter().any(|m| m.content.contains("parser")));
        assert!(out.messages.iter().any(|m| m.content.contains("ui")));
    }

    // AC-11.6.6 — diagnostics populated.
    #[test]
    fn diagnostics_are_populated() {
        let c = two_topic_conv();
        let out = WindowingAssembler::default().assemble(&c, big());
        assert!(out.diagnostics.group_count >= 2);
        assert!(out.diagnostics.active_group_id != GroupId(0));
        // gist overhead is small vs the verbatim turns it replaced → positive save
        assert!(out.diagnostics.tokens_saved_vs_passthrough >= 0);
    }

    // Degenerate: single group behaves like passthrough but reports counts.
    #[test]
    fn single_group_is_passthrough_with_counts() {
        let c = conv(vec![
            assistant_turn(0, "a", &[("Read", "src/a.rs")]),
            assistant_turn(1000, "b", &[("Read", "src/a.rs")]),
        ]);
        let out = WindowingAssembler::default().assemble(&c, big());
        let baseline = build_api_messages(&c);
        assert_eq!(out.messages.len(), baseline.len());
        assert_eq!(out.diagnostics.group_count, 1);
        assert_eq!(out.diagnostics.tokens_saved_vs_passthrough, 0);
    }

    #[test]
    fn empty_conversation_never_panics() {
        let c = Conversation::default();
        let out = WindowingAssembler::default().assemble(&c, big());
        assert_eq!(out.diagnostics.group_count, 0);
        assert!(out.messages.is_empty());
    }

    // AC-11.6.4 — tiny budget drops cold gists; active block always survives.
    #[test]
    fn tiny_budget_trims_cold_gists_keeps_active() {
        let c = two_topic_conv();
        let tiny = AssemblyBudget { max_tokens: 0 };
        let out = WindowingAssembler::default().assemble(&c, tiny);
        // No cold gist survives a zero budget.
        let gists = out
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .count();
        assert_eq!(gists, 0);
        assert!(out.diagnostics.truncated);
        // Active turns still present (never trimmed).
        assert!(out.messages.iter().any(|m| m.content.contains("parser")));
    }

    // AC-11.6.9a — overlap beats recency: an older cold group overlapping the
    // active file-set is retained over a newer disjoint one under a 1-gist budget.
    #[test]
    fn relevance_retention_prefers_overlap_over_recency() {
        // Group layout (time gaps force 3 groups):
        //   G0 (oldest): touches src/active/x.rs — OVERLAPS the active anchor
        //   G1 (newer):  touches src/unrelated/y.rs — DISJOINT
        //   G2 (active): touches src/active/x.rs
        let c = conv(vec![
            assistant_turn(0, "overlap old", &[("Read", "src/active/x.rs")]),
            assistant_turn(1000, "overlap old2", &[("Read", "src/active/x.rs")]),
            assistant_turn(60 * MIN, "unrelated", &[("Read", "src/unrelated/y.rs")]),
            assistant_turn(
                60 * MIN + 1000,
                "unrelated2",
                &[("Read", "src/unrelated/y.rs")],
            ),
            assistant_turn(120 * MIN, "active", &[("Read", "src/active/x.rs")]),
            assistant_turn(120 * MIN + 1000, "active2", &[("Read", "src/active/x.rs")]),
        ]);
        let asm = WindowingAssembler::default();
        let groups = group_turns(&c.turns, &GroupingConfig::default());
        assert!(groups.len() >= 3, "need ≥3 groups, got {}", groups.len());

        // Budget tuned to keep exactly ONE cold gist: active + one gist.
        let full = asm.assemble(&c, big());
        // total gist tokens of cold groups present at full budget:
        let cold_gist_msgs: Vec<&Message> = full
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .collect();
        assert!(cold_gist_msgs.len() >= 2);
        let one_gist_tokens = message_tokens(cold_gist_msgs[0]);
        // active tokens = full bundle minus all cold gist tokens
        let active_tokens: usize = full.messages.iter().map(message_tokens).sum::<usize>()
            - cold_gist_msgs
                .iter()
                .map(|m| message_tokens(m))
                .sum::<usize>();
        let budget = AssemblyBudget {
            max_tokens: active_tokens + one_gist_tokens,
        };
        let out = asm.assemble(&c, budget);
        let kept: Vec<&Message> = out
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .collect();
        assert_eq!(kept.len(), 1, "budget should keep exactly one cold gist");
        // The retained gist must be the OVERLAPPING group (G0), identified by its id.
        let overlap_group = &groups[0]; // src/active/x.rs
        assert!(
            kept[0]
                .content
                .contains(&format!("[group {}:", overlap_group.id.0)),
            "expected the overlapping group retained; kept: {}",
            kept[0].content
        );
    }

    // AC-11.6.9b — no overlap anywhere → trim degrades to pure recency (oldest
    // cold group drops first).
    #[test]
    fn pure_recency_fallback_drops_oldest_when_no_overlap() {
        // All cold groups touch files DISJOINT from the active anchor, so overlap
        // is 0 for every cold group and ordering reduces to recency.
        let c = conv(vec![
            assistant_turn(0, "old", &[("Read", "a/old1.rs")]),
            assistant_turn(1000, "old2", &[("Read", "a/old2.rs")]),
            assistant_turn(60 * MIN, "mid", &[("Read", "b/mid1.rs")]),
            assistant_turn(60 * MIN + 1000, "mid2", &[("Read", "b/mid2.rs")]),
            assistant_turn(120 * MIN, "active", &[("Read", "z/active.rs")]),
            assistant_turn(120 * MIN + 1000, "active2", &[("Read", "z/active2.rs")]),
        ]);
        let asm = WindowingAssembler::default();
        let groups = group_turns(&c.turns, &GroupingConfig::default());
        assert!(groups.len() >= 3);
        let full = asm.assemble(&c, big());
        let cold_gist_msgs: Vec<&Message> = full
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .collect();
        assert!(cold_gist_msgs.len() >= 2);
        let one_gist_tokens = message_tokens(cold_gist_msgs[0]);
        let active_tokens: usize = full.messages.iter().map(message_tokens).sum::<usize>()
            - cold_gist_msgs
                .iter()
                .map(|m| message_tokens(m))
                .sum::<usize>();
        // Keep exactly one cold gist → the most-recent cold group (G1) survives,
        // the oldest (G0) drops first.
        let budget = AssemblyBudget {
            max_tokens: active_tokens + one_gist_tokens,
        };
        let out = asm.assemble(&c, budget);
        let kept: Vec<&Message> = out
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .collect();
        assert_eq!(kept.len(), 1);
        let newer_cold = &groups[1]; // the more recent cold group
        assert!(
            kept[0]
                .content
                .contains(&format!("[group {}:", newer_cold.id.0)),
            "expected the newer cold group retained (pure recency); kept: {}",
            kept[0].content
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Topic-return scenario (user request).
    //
    //   pattern: A A A A  B B B B  A A A A  C C C C  B B B  A A A A
    //            └─ G0 ─┘ └─ G1 ─┘ └─ G2 ─┘ └─ G3 ─┘ └ G4 ┘ └─ G5 ─┘
    //                                                          (active)
    //
    // Each letter = a turn whose tool touches that topic's file — the file-set
    // is how the grouper tells topics apart (R4 file-drift). The session ENDS
    // back on topic A, so the active anchor is topic A. The earlier A history
    // (G0, G2) is resurfaced as GISTS (not verbatim turns) and, under budget
    // pressure, is retained in preference to the interleaved B / C groups —
    // even though some B / C groups are more recent. That is the real
    // mechanism behind "returning to A brings back the prior A topics".
    // ─────────────────────────────────────────────────────────────────────

    /// Build the A/B/A/C/B/A branching conversation. Turns are 1s apart (so the
    /// R3 time-gap never fires) and all use the same `Read` tool (so the R5
    /// tool-break never fires) — the ONLY boundary driver is R4 file-set drift
    /// between topics, giving 6 clean single-topic groups.
    fn topic_return_conv() -> Conversation {
        let seg = |ts0: i64, n: usize, prose: &str, file: &str| -> Vec<Turn> {
            (0..n)
                .map(|i| assistant_turn(ts0 + i as i64 * 1000, prose, &[("Read", file)]))
                .collect()
        };
        let a = "src/topic_a.rs";
        let b = "src/topic_b.rs";
        let cc = "src/topic_c.rs";
        let mut turns = Vec::new();
        turns.extend(seg(0, 4, "work on A", a)); // G0  ts 0..3000
        turns.extend(seg(4_000, 4, "work on B", b)); // G1  ts 4000..7000
        turns.extend(seg(8_000, 4, "back to A", a)); // G2  ts 8000..11000
        turns.extend(seg(12_000, 4, "work on C", cc)); // G3  ts 12000..15000
        turns.extend(seg(16_000, 3, "B again", b)); // G4  ts 16000..18000
        turns.extend(seg(19_000, 4, "return to A", a)); // G5  ts 19000..22000 (active)
        conv(turns)
    }

    /// Parse the group id out of a rendered cold-gist message
    /// (`[group {id}: {body}]`).
    fn gist_group_id(content: &str) -> Option<u64> {
        content
            .strip_prefix("[group ")
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse::<u64>().ok())
    }

    // User scenario, part 1 — relevance retention under budget: ending on topic
    // A, the two EARLIER topic-A groups survive the trim while the interleaved
    // B / C groups drop first (overlap beats recency).
    #[test]
    fn returning_to_topic_a_resurfaces_prior_a_groups_over_b_and_c() {
        use std::collections::BTreeSet;
        use std::path::PathBuf;

        let c = topic_return_conv();
        let asm = WindowingAssembler::default();
        let groups = group_turns(&c.turns, &GroupingConfig::default());

        // Grouping invariant (R4 file-Jaccard / R5 tool-break): the six contiguous
        // single-file segments must split into EXACTLY six groups whose file-sets
        // follow the authored topic order A, B, A, C, B, A — a boundary at every
        // topic change, no spurious merge and no extra split. A regression in the
        // grouping rule fails HERE, not just on a loose `len() >= 6` floor.
        let a_file = PathBuf::from("src/topic_a.rs");
        let b_file = PathBuf::from("src/topic_b.rs");
        let c_file = PathBuf::from("src/topic_c.rs");
        let one = |p: &PathBuf| BTreeSet::from([p.clone()]);
        let file_seq: Vec<BTreeSet<PathBuf>> = groups
            .iter()
            .map(|g| g.signature.file_set.iter().cloned().collect())
            .collect();
        assert_eq!(
            file_seq,
            vec![
                one(&a_file),
                one(&b_file),
                one(&a_file),
                one(&c_file),
                one(&b_file),
                one(&a_file),
            ],
            "grouping must split on every topic change in order A,B,A,C,B,A; got {file_seq:?}"
        );

        let anchor = groups.last().unwrap();
        assert!(
            anchor.signature.file_set.contains(&a_file),
            "the active anchor (last group) must be topic A"
        );

        // The earlier (cold) topic-A groups — everything but the active one.
        let a_cold_ids: BTreeSet<u64> = groups[..groups.len() - 1]
            .iter()
            .filter(|g| g.signature.file_set.contains(&a_file))
            .map(|g| g.id.0)
            .collect();
        assert_eq!(
            a_cold_ids.len(),
            2,
            "expected two earlier topic-A groups (G0, G2)"
        );

        // Full budget → every cold group is present as a gist (sanity baseline).
        let full = asm.assemble(&c, big());
        let cold_at_full: Vec<&Message> = full
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .collect();
        assert_eq!(
            cold_at_full.len(),
            groups.len() - 1,
            "all cold groups present at full budget"
        );

        // Budget = active block + exactly the two topic-A gists. The B / C gists
        // (zero file-overlap with the A anchor) score lower and must be trimmed
        // first, leaving precisely the two resurfaced A topics.
        let cold_total: usize = cold_at_full.iter().map(|m| message_tokens(m)).sum();
        let active_tokens: usize =
            full.messages.iter().map(message_tokens).sum::<usize>() - cold_total;
        let a_gist_tokens: usize = cold_at_full
            .iter()
            .filter(|m| gist_group_id(&m.content).is_some_and(|id| a_cold_ids.contains(&id)))
            .map(|m| message_tokens(m))
            .sum();

        let budget = AssemblyBudget {
            max_tokens: active_tokens + a_gist_tokens,
        };
        let out = asm.assemble(&c, budget);
        assert!(out.diagnostics.truncated, "the tight budget should force a trim");

        let kept_ids: BTreeSet<u64> = out
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("[group "))
            .filter_map(|m| gist_group_id(&m.content))
            .collect();

        assert_eq!(
            kept_ids, a_cold_ids,
            "only the two earlier topic-A groups should survive the trim; kept {kept_ids:?}, \
             expected {a_cold_ids:?}"
        );
    }

    // User scenario, part 2 — explicit continuation link (budget-independent):
    // the returning A group's gist is prefixed [cont. group #<first A>] because
    // its file-set Jaccard with the earlier A group is ≥ 0.7 (AC-11.6.10).
    #[test]
    fn returning_to_topic_a_is_marked_as_continuation_of_prior_a() {
        use std::path::PathBuf;

        let c = topic_return_conv();
        let groups = group_turns(&c.turns, &GroupingConfig::default());
        let a_file = PathBuf::from("src/topic_a.rs");
        let a_groups: Vec<&TurnGroup> = groups
            .iter()
            .filter(|g| g.signature.file_set.contains(&a_file))
            .collect();
        assert!(a_groups.len() >= 2, "need ≥2 topic-A groups");

        let first_a = a_groups[0];
        let second_a = a_groups[1];
        assert_eq!(
            second_a.supersedes,
            Some(first_a.id),
            "the second topic-A group should supersede the first"
        );
        assert!(
            second_a
                .gist
                .starts_with(&format!("[cont. group #{}]", first_a.id.0)),
            "the returning A group's gist should carry the continuation marker; got: {}",
            second_a.gist
        );
    }
}
