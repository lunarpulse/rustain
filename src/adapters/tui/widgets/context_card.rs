//! Read-only `/context show` card (Story 11.4, AC6).
//!
//! Renders the last-assembled [`ContextBundle`] as a header + one row per source
//! with its per-source token count + a total, plus dedup / truncation notes. It
//! is **read-only** — no accept/decline round-trip (unlike `consolidation_card.rs`)
//! — and is surfaced as a `SystemNotice` (so it persists in scrollback) rather
//! than an interactive inline card, keeping the Story-11.4 `/context` command
//! surface within the STOP-and-flag budget (1 widget helper + 1 toggle bool + 1
//! status segment). Pure text → no theme/ratatui dependency → trivially testable.

use crate::domain::models::ContextBundle;

/// Build the read-only context card text (newline-joined) for `/context show`.
pub fn context_card_text(bundle: &ContextBundle, injection_on: bool) -> String {
    let d = &bundle.diagnostics;
    let mut lines: Vec<String> = Vec::new();

    let n = d.per_source_tokens.len();
    lines.push(format!(
        "\u{1F4CB} Injected context — {} source{}, ~{} token{}{}",
        n,
        if n == 1 { "" } else { "s" },
        d.total_tokens,
        if d.total_tokens == 1 { "" } else { "s" },
        if injection_on {
            ""
        } else {
            " (injection OFF — /context on to enable)"
        },
    ));

    if d.per_source_tokens.is_empty() {
        lines.push("  (no context assembled this turn)".to_string());
    } else {
        for (source, toks) in &d.per_source_tokens {
            let note = if source.is_injectable() {
                ""
            } else {
                " — reference only (injected by persona)"
            };
            lines.push(format!("  {} — ~{toks} tokens{note}", source.attribution()));
        }
    }

    if d.deduped_count > 0 {
        lines.push(format!(
            "  ({} duplicate{} removed across sources)",
            d.deduped_count,
            if d.deduped_count == 1 { "" } else { "s" }
        ));
    }
    if d.truncated {
        lines.push(
            "  (truncated to fit the token budget — lowest-priority sources dropped first)"
                .to_string(),
        );
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        AssembleDiagnostics, ContextSource, ProvenancedEntry, Relevance, RetrievalMethod,
    };
    use chrono::NaiveDate;
    use std::sync::Arc;

    fn bundle() -> ContextBundle {
        let date = NaiveDate::from_ymd_opt(2026, 3, 25).unwrap();
        ContextBundle {
            entries: vec![
                ProvenancedEntry {
                    source: ContextSource::MemoryMd,
                    content: Arc::from("prefers snake_case"),
                    timestamp: 0,
                    retrieval_method: RetrievalMethod::MemoryMd,
                    relevance: Relevance::Unscored,
                },
                ProvenancedEntry {
                    source: ContextSource::Project("CLAUDE.md".into()),
                    content: Arc::from("rules"),
                    timestamp: 0,
                    retrieval_method: RetrievalMethod::Structural,
                    relevance: Relevance::Unscored,
                },
            ],
            diagnostics: AssembleDiagnostics {
                per_source_tokens: vec![
                    (ContextSource::MemoryMd, 5),
                    (ContextSource::Project("CLAUDE.md".into()), 3),
                    (ContextSource::DailyLog(date), 4),
                ],
                total_tokens: 9,
                truncated: true,
                deduped_count: 2,
            },
        }
    }

    #[test]
    fn card_shows_per_source_token_rows_and_total() {
        let text = context_card_text(&bundle(), true);
        assert!(text.contains("3 sources"));
        assert!(text.contains("~9 tokens"));
        assert!(text.contains("[memory] — ~5 tokens"));
        assert!(text.contains("[daily-log: 2026-03-25] — ~4 tokens"));
        // Project is shown but flagged as persona-injected (reference only).
        assert!(
            text.contains(
                "[project: CLAUDE.md] — ~3 tokens — reference only (injected by persona)"
            )
        );
        assert!(text.contains("2 duplicates removed"));
        assert!(text.contains("truncated to fit the token budget"));
    }

    #[test]
    fn card_notes_injection_off() {
        let text = context_card_text(&bundle(), false);
        assert!(text.contains("injection OFF"));
    }

    #[test]
    fn empty_bundle_card_says_nothing_assembled() {
        let text = context_card_text(&ContextBundle::empty(), true);
        assert!(text.contains("no context assembled this turn"));
        assert!(text.contains("0 sources"));
    }
}
