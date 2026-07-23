//! `SynthesisBlock` — the grounded-synthesis HERO widget (Story 14.3, AC7).
//!
//! Renders at **render index 0** (the caller places these lines first + in
//! full, without expansion). Carries the honest coverage line + per-spoke
//! citations + an explicit honest-empty state. Drill is optional (the full
//! payloads live in the `ResultStore`, fetched lazily — never inlined here).
//!
//! Mirrors the `plan_card` / `consolidation_card` grammar: a free function
//! returning `Vec<Line<'static>>` for inline rendering in the chat pane (no
//! direct `Frame` mutation), so it composes with the existing render pipeline.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::domain::models::SpokeResult;
use crate::domain::models::orchestration::ForkJoinOutcome;

/// Render the synthesis HERO lines. The caller places these at render index 0.
///
/// - Honest coverage line (e.g. `over 12 of 15 — 2 failed, 1 empty`).
/// - Per-spoke citations (one per completed spoke — no orphan claims, AC7).
/// - Explicit honest-empty state when zero spokes contributed signal (the
///   "cruelest lie" guard — never confident noise when all failed/empty).
pub fn render_synthesis_block_lines(outcome: &ForkJoinOutcome) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let header_style = Style::default().add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(Color::DarkGray);
    let warn = Style::default().fg(Color::Yellow);
    let ok = Style::default().fg(Color::Green);

    // HERO header: the grounded summary, first + in full. The summary already
    // encodes the honest-empty intent (P21: no redundant hardcoded "no signal"
    // literal — the summary IS the single statement).
    if outcome.synthesis.honest_empty {
        lines.push(Line::styled(
            format!("\u{25C6} Synthesis \u{2014} {}", outcome.synthesis.summary),
            warn,
        ));
    } else {
        lines.push(Line::styled(
            outcome.synthesis.summary.clone(),
            header_style,
        ));
    }

    // Coverage line (voiced first per a11y — it is the second visual line).
    let coverage = outcome.synthesis.coverage.render();
    let coverage_style = if outcome.synthesis.honest_empty || outcome.synthesis.coverage.failed > 0
    {
        warn
    } else {
        ok
    };
    lines.push(Line::styled(format!("  {coverage}"), coverage_style));

    // Per-spoke citations (one per completed spoke). Length == coverage.completed
    // by the SynthesisView::build postcondition — no orphan claims.
    for cite in &outcome.synthesis.citations {
        lines.push(Line::from(vec![
            Span::styled("  \u{2039}", muted),
            Span::styled(cite.label.clone(), header_style),
            Span::styled("\u{203A} ", muted),
            Span::raw(cite.summary.clone()),
        ]));
    }

    // Degraded spokes are listed (not hidden) so the human sees what failed.
    // P21: render the agent_id via its inner string (not Debug), and surface a
    // sanitized reason (the SpokeResult already carries a stable category).
    for (agent_id, result) in &outcome.spokes {
        if matches!(result, SpokeResult::Completed { .. }) {
            continue;
        }
        let glyph = crate::adapters::tui::widgets::orchestration_glyph::spoke_result_glyph(result);
        let detail = match result {
            SpokeResult::Failed { reason } => format!(" \u{2014} {reason}"),
            SpokeResult::Cancelled => String::from(" \u{2014} cancelled"),
            SpokeResult::Empty => String::from(" \u{2014} empty"),
            SpokeResult::Completed { .. } => String::new(),
        };
        lines.push(Line::from(vec![
            Span::styled("  ", muted),
            Span::styled(glyph.to_string(), warn),
            Span::raw(format!(" {}{detail}", agent_id.as_str())),
        ]));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::AgentId;
    use crate::domain::models::orchestration::{CoverageLine, SpokeCitation, SynthesisView};

    fn outcome_with(citations: Vec<SpokeCitation>, coverage: CoverageLine) -> ForkJoinOutcome {
        let synthesis = SynthesisView::build(citations, coverage);
        ForkJoinOutcome {
            spokes: vec![],
            synthesis,
        }
    }

    #[test]
    fn renders_coverage_line_and_citations() {
        let coverage = CoverageLine {
            completed: 2,
            failed: 1,
            cancelled: 0,
            empty: 0,
            total: 3,
        };
        let citations = vec![
            SpokeCitation {
                agent_id: AgentId::from_validated("a"),
                label: "alpha".into(),
                summary: "sa".into(),
            },
            SpokeCitation {
                agent_id: AgentId::from_validated("b"),
                label: "beta".into(),
                summary: "sb".into(),
            },
        ];
        let outcome = outcome_with(citations, coverage);
        let lines = render_synthesis_block_lines(&outcome);
        // HERO header first.
        assert!(lines[0].spans.iter().any(|s| !s.content.is_empty()));
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<String>();
        assert!(rendered.contains("over 2 of 3"));
        assert!(rendered.contains("alpha") && rendered.contains("beta"));
    }

    /// AC7 keystone `synthesis_renders_first`: the SynthesisBlock HERO renders
    /// FIRST and IN FULL without expansion. The synthesis summary is line[0]
    /// (before coverage, before citations); the synthesis block is a self-
    /// contained, no-expansion render (a single call yields the complete
    /// synthesis view). A mutant that put coverage first would fail the
    /// line[0] assertion. (Review finding AC7: no test proved SynthesisBlock
    /// renders first/in-full without expansion — this is that test.)
    #[test]
    fn ac7_synthesis_renders_first_and_in_full_without_expansion() {
        let coverage = CoverageLine {
            completed: 2,
            failed: 0,
            cancelled: 0,
            empty: 0,
            total: 2,
        };
        let citations = vec![
            SpokeCitation {
                agent_id: AgentId::from_validated("a"),
                label: "alpha".into(),
                summary: "alpha summary".into(),
            },
            SpokeCitation {
                agent_id: AgentId::from_validated("b"),
                label: "beta".into(),
                summary: "beta summary".into(),
            },
        ];
        let outcome = outcome_with(citations, coverage);
        // ONE call renders the synthesis HERO in full — no expansion needed.
        let lines = render_synthesis_block_lines(&outcome);
        assert!(!lines.is_empty(), "synthesis block is non-empty");
        // HERO header first: the synthesis summary is line[0] (the FIRST line
        // a user sees — synthesis renders at render index 0).
        let first_line: String = lines[0].spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            first_line.contains("Synthesized") || first_line.contains("Synthesis"),
            "synthesis HERO header is line[0] (renders first): {first_line:?}"
        );
        // In full without expansion: a single render_synthesis_block_lines call
        // produces the synthesis summary + coverage + per-spoke citations in
        // ONE pass. The total visible content is the synthesis summary, the
        // coverage line, and both per-spoke citations.
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<String>();
        assert!(
            all.contains("over 2 of 2"),
            "coverage line rendered in the same pass: {all:?}"
        );
        assert!(
            all.contains("alpha") && all.contains("alpha summary"),
            "citation `alpha` rendered in the same pass"
        );
        assert!(
            all.contains("beta") && all.contains("beta summary"),
            "citation `beta` rendered in the same pass"
        );
        // The synthesis summary precedes the citations (HERO is first).
        let summary_idx = all.find("Synthesized").or_else(|| all.find("Synthesis"));
        let alpha_idx = all.find("alpha summary");
        assert!(
            summary_idx.is_some() && alpha_idx.is_some() && summary_idx < alpha_idx,
            "synthesis HERO header precedes the citations (renders first): \
             summary_idx={summary_idx:?}, alpha_idx={alpha_idx:?}"
        );
    }
    #[test]
    fn honest_empty_renders_explicit_no_signal_state() {
        let coverage = CoverageLine {
            completed: 0,
            failed: 1,
            cancelled: 1,
            empty: 0,
            total: 2,
        };
        let outcome = outcome_with(vec![], coverage);
        let lines = render_synthesis_block_lines(&outcome);
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<String>();
        assert!(rendered.contains("no spoke contributed"));
    }
}
