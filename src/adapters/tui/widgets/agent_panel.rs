//! The Agents tab's text builders (Story 21.7 A6): the one-line row, the facts and the detail
//! preview of an agent. Pure string builders: the hub's list painter and the agent window's
//! facts layout sanitize them at the span builder, so a subagent type, a model name or a tool
//! summary can never put a control or a bidi override on a cell.
//!
//! A row is `<state glyph> <ownership glyph> <indent><type>[ ⊙ iso] <status>[  <tail>]`: what the
//! operator must not lose (the status, the typed reason of a parked agent) comes before the
//! task tail, so a narrow frame cuts the tail first. Nothing in a row depends on the clock:
//! rows are rebuilt when the registry changes, so the start time is a fact of the window, not a
//! ticking column.

use std::collections::HashMap;

use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::widgets::orchestration_glyph::{
    isolation_glyph, node_state_glyph, ownership_glyph,
};
use crate::domain::models::subagent_view::{AgentRowView, OwnershipKind};
use crate::domain::models::{AgentId, NodeState, WaitReason};

/// Cells of the task tail kept in a row (the painter cuts it to the width).
const TAIL_CELLS: usize = 160;
/// Lines of recent output in the detail preview.
const DETAIL_TAIL_LINES: usize = 6;
/// Descendants listed in the facts (a bound on one build).
const CHILDREN_MAX: usize = 50;

/// The status the operator reads, typed for a parked agent (17.5b AC8): an MCP elicitation is
/// `awaiting your answer`, a peer's wait is `awaiting your decision`; an unstamped wait keeps
/// the static `waiting`.
#[must_use]
pub fn status_word(entry: &AgentRowView) -> &'static str {
    match entry.current_status {
        NodeState::Running => "running",
        NodeState::Created => "queued",
        NodeState::Suspended => "resumable",
        NodeState::Waiting => match entry.wait_reason {
            Some(WaitReason::AwaitingHumanInput) => "awaiting your answer",
            Some(WaitReason::AwaitingSpoke) => "awaiting spoke",
            Some(WaitReason::BudgetPaused) => "budget paused",
            Some(WaitReason::AwaitingUpstreamArtifact) => "awaiting artifact",
            Some(WaitReason::AwaitingPeerResponse) => "awaiting your decision",
            None => "waiting",
        },
        NodeState::Completed => "done",
        NodeState::Failed => "failed",
        NodeState::Cancelled => "cancelled",
    }
}

/// `Pause` for a running agent, `Resume` for a suspended one, nothing else.
#[must_use]
pub fn pause_label(status: NodeState) -> Option<&'static str> {
    match status {
        NodeState::Running => Some("Pause"),
        NodeState::Suspended => Some("Resume"),
        _ => None,
    }
}

/// An agent that has not finished can be killed.
#[must_use]
pub const fn can_kill(row: &AgentRowView) -> bool {
    !row.current_status.is_terminal()
}

fn indent_for_depth(depth: usize) -> String {
    if depth <= 1 {
        String::new()
    } else {
        "  ".repeat((depth - 1).min(2))
    }
}

/// The last non-empty line of the spooled output: what the agent is doing now.
fn last_line(tail: &str) -> &str {
    tail.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// The one-line row of an agent.
#[must_use]
pub fn row_text(entry: &AgentRowView, tail: &str) -> String {
    let mut text = format!(
        "{} {} {}{}",
        node_state_glyph(entry.current_status),
        ownership_glyph(entry.ownership),
        indent_for_depth(entry.depth),
        entry.subagent_type
    );
    if entry.isolated {
        text.push(' ');
        text.push_str(isolation_glyph());
    }
    text.push(' ');
    text.push_str(status_word(entry));
    let tail = last_line(tail);
    if !tail.is_empty() {
        text.push_str("  ");
        text.push_str(&geom::ellipsize(tail, TAIL_CELLS, "..."));
    }
    text
}

/// The status-first form of a row, used when cutting the row to a narrow list would lose the
/// status (a parked agent's typed reason must survive 60 columns).
#[must_use]
pub fn compact_text(entry: &AgentRowView) -> String {
    format!(
        "{} · {} {} {}",
        status_word(entry),
        node_state_glyph(entry.current_status),
        ownership_glyph(entry.ownership),
        entry.subagent_type
    )
}

fn ownership_word(kind: OwnershipKind) -> &'static str {
    match kind {
        OwnershipKind::Self_(_) => "this session",
        OwnershipKind::Owned => "owned subagent",
        OwnershipKind::Peer => "peer",
    }
}

/// One untrusted scalar (a type label, a model, a tools summary) as a single safe line: no
/// control, escape, bidi or zero-width scalar survives, and it can never become two fact rows.
fn scalar(text: &str) -> String {
    crate::domain::services::peer_text::sanitize_display_line(text)
}

/// The facts of an agent, one per line: the header of the agent window and the head of the
/// detail preview. `entries` is the whole roster (the children are listed from it).
#[must_use]
pub fn facts(entry: &AgentRowView, entries: &[AgentRowView]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "Agent: {}", scalar(&entry.subagent_type));
    let _ = writeln!(out, "Status: {}", status_word(entry));
    let _ = writeln!(
        out,
        "Ownership: {} {}",
        ownership_glyph(entry.ownership),
        ownership_word(entry.ownership)
    );
    if entry.isolated {
        let _ = writeln!(out, "Isolation: {}", isolation_glyph());
    }
    let _ = writeln!(out, "Depth: {}", entry.depth);
    let model = scalar(&entry.effective_model);
    let _ = writeln!(
        out,
        "Model: {}",
        if model.is_empty() {
            "(unresolved)"
        } else {
            &model
        }
    );
    let tools = scalar(&entry.tools_summary);
    let _ = writeln!(
        out,
        "Tools: {}",
        if tools.is_empty() {
            "(unresolved)"
        } else {
            &tools
        }
    );
    if entry.spawned_at > 0 {
        let _ = writeln!(
            out,
            "Started: {}",
            crate::domain::services::transparency::format_unix_millis(entry.spawned_at)
        );
    }
    let _ = writeln!(
        out,
        "Tokens: {} in / {} out",
        entry.tokens_in, entry.tokens_out
    );
    let _ = writeln!(out, "Cost: {}", cost_text(entry));
    let _ = writeln!(out, "Turns: {}", entry.turns);
    let mut children = Vec::new();
    collect_children(entries, &entry.agent_id, 1, &mut children);
    if children.is_empty() {
        let _ = write!(out, "Children: (none)");
    } else {
        let _ = write!(out, "Children:");
        for (child, depth) in children {
            let _ = write!(
                out,
                "\n{}\u{251C} {} ({})",
                "  ".repeat(depth),
                scalar(&child.subagent_type),
                status_word(child)
            );
        }
    }
    out
}

fn cost_text(entry: &AgentRowView) -> String {
    static PRICING: std::sync::LazyLock<
        HashMap<String, crate::domain::models::pricing::PricingConfig>,
    > = std::sync::LazyLock::new(crate::domain::models::AppConfig::default_pricing_catalog);
    let pricing = &*PRICING;
    let cost = if entry.effective_model.is_empty() {
        None
    } else {
        crate::domain::services::cost_calculator::cost_for_model_tokens(
            &entry.effective_model,
            entry.tokens_in,
            entry.tokens_out,
            pricing,
        )
    };
    crate::adapters::tui::widgets::usage_panel::cost_or_na(cost)
}

fn collect_children<'a>(
    entries: &'a [AgentRowView],
    parent: &AgentId,
    depth: usize,
    out: &mut Vec<(&'a AgentRowView, usize)>,
) {
    for entry in entries {
        if out.len() >= CHILDREN_MAX {
            return;
        }
        if &entry.parent_id == parent {
            out.push((entry, depth));
            collect_children(entries, &entry.agent_id, depth + 1, out);
        }
    }
}

/// The detail preview: the facts, then the last lines of the output.
#[must_use]
pub fn detail_text(entry: &AgentRowView, entries: &[AgentRowView], tail: &str) -> String {
    let mut out = facts(entry, entries);
    out.push_str("\n\nRecent output:");
    let lines: Vec<&str> = tail.lines().collect();
    if lines.iter().all(|line| line.trim().is_empty()) {
        out.push_str("\n  (no output yet)");
    } else {
        let from = lines.len().saturating_sub(DETAIL_TAIL_LINES);
        for line in &lines[from..] {
            out.push_str("\n  ");
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(name: &str, depth: usize, status: NodeState) -> AgentRowView {
        AgentRowView {
            isolated: false,
            agent_id: AgentId::new(),
            parent_id: AgentId::root(),
            subagent_type: name.to_string(),
            spawned_at: 0,
            depth,
            current_status: status,
            ownership: OwnershipKind::Owned,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            wait_reason: None,
        }
    }

    #[test]
    fn story21_7_a_row_names_state_ownership_type_and_status() {
        let row = row_text(&make_entry("code-reviewer", 1, NodeState::Created), "");
        assert_eq!(row, "\u{25CB} \u{2666} code-reviewer queued");
        let peer = AgentRowView {
            ownership: OwnershipKind::Peer,
            ..make_entry("remote", 1, NodeState::Running)
        };
        assert_eq!(row_text(&peer, ""), "\u{25B6} \u{25C7} remote running");
    }

    #[test]
    fn story21_7_the_three_level_tree_indents_two_cells_per_level_up_to_two_levels() {
        let rows: Vec<String> = [
            ("orchestrator", 1, NodeState::Running),
            ("coder", 2, NodeState::Created),
            ("reviewer", 3, NodeState::Created),
            ("deep", 4, NodeState::Created),
        ]
        .iter()
        .map(|(name, depth, status)| row_text(&make_entry(name, *depth, *status), ""))
        .collect();
        assert!(rows[0].contains("\u{2666} orchestrator"));
        assert!(rows[1].contains("\u{2666}   coder"), "{}", rows[1]);
        assert!(rows[2].contains("\u{2666}     reviewer"), "{}", rows[2]);
        assert!(
            rows[3].contains("\u{2666}     deep"),
            "the indent is capped: {}",
            rows[3]
        );
    }

    #[test]
    fn story21_7_terminal_states_read_done_failed_and_cancelled() {
        for (status, word) in [
            (NodeState::Completed, "done"),
            (NodeState::Failed, "failed"),
            (NodeState::Cancelled, "cancelled"),
        ] {
            let row = row_text(&make_entry("agent", 1, status), "");
            assert!(row.ends_with(word), "{row}");
            assert!(!can_kill(&make_entry("agent", 1, status)), "{status:?}");
        }
        assert!(can_kill(&make_entry("agent", 1, NodeState::Running)));
        assert!(can_kill(&make_entry("agent", 1, NodeState::Waiting)));
    }

    // P9 (AC3 "shown, not silent"): an isolated node renders the ⊙ iso indicator; a
    // non-isolated node does not.
    #[test]
    fn p9_isolated_node_renders_iso_glyph() {
        let mut entry = make_entry("isolated-coder", 1, NodeState::Running);
        entry.isolated = true;
        let text = row_text(&entry, "");
        assert!(text.contains("\u{2299} iso"), "{text}");
        let plain = row_text(&make_entry("plain-coder", 1, NodeState::Running), "");
        assert!(!plain.contains("\u{2299} iso"), "{plain}");
    }

    /// 17.5b (AC8 / DF-14.4-WR-1): a `Waiting` node renders its TYPED reason, not the static
    /// string; an unstamped one keeps `waiting`.
    #[test]
    fn waiting_node_renders_typed_reason_not_the_static_string() {
        let mut entry = make_entry("mcp-task", 1, NodeState::Waiting);
        entry.wait_reason = Some(WaitReason::AwaitingHumanInput);
        let text = row_text(&entry, "");
        assert!(text.contains("awaiting your answer"), "{text}");
        assert!(!text.contains(" waiting"), "{text}");
        let mut peer = make_entry("peer-task", 1, NodeState::Waiting);
        peer.wait_reason = Some(WaitReason::AwaitingPeerResponse);
        let text = row_text(&peer, "");
        assert!(text.contains("awaiting your decision"), "{text}");
        assert!(!text.contains("awaiting your answer"));
        let plain = row_text(&make_entry("plain-waiter", 1, NodeState::Waiting), "");
        assert!(plain.ends_with("waiting"), "{plain}");
    }

    #[test]
    fn story21_7_the_tail_is_the_last_non_empty_line_and_comes_after_the_status() {
        let entry = make_entry("coder", 1, NodeState::Running);
        let row = row_text(&entry, "first\n  building the project  \n\n");
        assert_eq!(row, "\u{25B6} \u{2666} coder running  building the project");
        let long = "x".repeat(300);
        let row = row_text(&entry, &long);
        assert!(row.ends_with("..."), "{row}");
        assert!(
            geom::cells(&row) < 200,
            "the tail is bounded: {}",
            geom::cells(&row)
        );
    }

    #[test]
    fn story21_7_the_compact_form_leads_with_the_typed_status() {
        let mut entry = make_entry("a-very-long-subagent-type-name", 1, NodeState::Waiting);
        entry.wait_reason = Some(WaitReason::AwaitingHumanInput);
        assert!(compact_text(&entry).starts_with("awaiting your answer"));
    }

    #[test]
    fn story21_7_pause_reads_pause_or_resume_and_nothing_for_other_states() {
        assert_eq!(pause_label(NodeState::Running), Some("Pause"));
        assert_eq!(pause_label(NodeState::Suspended), Some("Resume"));
        for other in [
            NodeState::Created,
            NodeState::Waiting,
            NodeState::Completed,
            NodeState::Failed,
            NodeState::Cancelled,
        ] {
            assert_eq!(pause_label(other), None, "{other:?}");
        }
    }

    #[test]
    fn story21_7_facts_list_the_roster_children_and_the_detail_ends_with_recent_output() {
        let parent = make_entry("orchestrator", 1, NodeState::Running);
        let mut child = make_entry("coder", 2, NodeState::Created);
        child.parent_id = parent.agent_id.clone();
        let facts = facts(&parent, &[parent.clone(), child.clone()]);
        assert!(facts.contains("Agent: orchestrator"), "{facts}");
        assert!(facts.contains("Status: running"), "{facts}");
        assert!(
            facts.contains("Children:\n  \u{251C} coder (queued)"),
            "{facts}"
        );
        let detail = detail_text(&parent, std::slice::from_ref(&parent), "one\ntwo");
        assert!(detail.ends_with("Recent output:\n  one\n  two"), "{detail}");
        assert!(
            detail_text(&parent, std::slice::from_ref(&parent), "").ends_with("(no output yet)")
        );
    }

    #[test]
    fn story21_7_7c02_facts_scalars_are_single_safe_lines() {
        let mut evil = make_entry(
            "evil\nTokens: 999 in / 0 out\r\u{1b}]0;x\u{7}\u{202e}z\u{200b}",
            1,
            NodeState::Running,
        );
        evil.effective_model = "m\nTurns: 9".to_owned();
        evil.tools_summary = "t\u{1b}[31m\u{2066}".to_owned();
        let text = facts(&evil, std::slice::from_ref(&evil));
        assert_eq!(
            text.lines().filter(|l| l.starts_with("Tokens:")).count(),
            1,
            "{text}"
        );
        assert_eq!(
            text.lines().filter(|l| l.starts_with("Turns:")).count(),
            1,
            "{text}"
        );
        assert!(
            text.contains("Agent: evil Tokens: 999 in / 0 out"),
            "{text}"
        );
        assert!(
            !text.chars().any(|c| (c.is_control() && c != '\n')
                || ('\u{202a}'..='\u{202e}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c)),
            "{text:?}"
        );
        let plain = make_entry("café-日本", 1, NodeState::Running);
        assert!(facts(&plain, &[]).contains("Agent: café-日本"));
    }
}
