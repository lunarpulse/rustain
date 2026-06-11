//! Pure memory-consolidation service — Story 11.2a (completes Story 11.2 AC4) + 12.2d (AC8 shared generation).
//!
//! Pure helpers + the model-facing system prompt + shared generation function:
//! - [`build_proposal_prompt`] renders recent daily-log entries into the
//!   user-message body for a background model sub-turn (mirrors
//!   `compaction::build_compaction_prompt_input` — bounded token cost).
//! - [`parse_proposals`] tolerantly parses the model's JSON reply into
//!   `MemoryFact`s.
//! - [`generate_proposals`] runs a structured model sub-turn and returns proposed
//!   facts (shared by the TUI handler and the daemon attach path, extracted Story 12.2d AC8).
//!
//! domain/services discipline: the helpers are PURE — no I/O, no async, string in /
//! `Vec<MemoryFact>` out. `generate_proposals` IS async (streaming provider call) but
//! carries no adapter dependencies — it depends only on the `StreamingProvider` port.
//! The JSON parse maps through a PRIVATE `#[derive(Deserialize)] ProposedFact` DTO so
//! `MemoryFact` itself stays serde-free for on-disk use (the markdown round-trip is its
//! contract — 11.2 Q1). Wire-protocol serialization uses the serde derives added in 12.2d.

use std::time::Duration;

use crate::domain::models::{MemoryEntry, MemoryFact};

/// Cap on recent entries fed to the model (token bound).
const MAX_ENTRIES: usize = 30;
/// Cap on characters of any single entry field included in the prompt.
const MAX_ENTRY_CHARS: usize = 600;
/// Cap on proposals accepted from one model response (defensive bound).
const MAX_PROPOSALS: usize = 20;

/// Background task timeout for consolidation generation — mirrors
/// `event_loop.rs::BACKGROUND_TASK_TIMEOUT` (10s). Shared between the TUI handler
/// and the daemon attach path (Story 12.2d AC8).
pub const CONSOLIDATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Model-facing instruction for the consolidation sub-turn. Deterministic and
/// tight by design — flakiness > realism (project test-prompt principle).
pub const CONSOLIDATION_SYSTEM_PROMPT: &str = "\
You are the memory-consolidation assistant for a terminal coding agent. You are \
shown recent activity-log entries. Identify ONLY genuinely DURABLE, RECURRING \
facts, preferences, or decisions worth promoting into long-term project memory \
(MEMORY.md). Ignore one-off operational noise, transient state, and anything \
that would not still be useful weeks from now.\n\n\
Respond with ONLY a JSON array — no prose, no markdown code fences. Each element \
is an object:\n\
  {\"category\": string, \"fact\": string, \"detail\": string (optional)}\n\
- category: a short topic header (e.g. \"Preferences\", \"Architecture\", \"Build\").\n\
- fact: one concise, self-contained sentence.\n\
- detail: optional supporting context.\n\n\
Return only items worth keeping. If nothing is worth promoting, return [].";

/// Render recent entries (newest-first as supplied) into the sub-turn user body.
/// Caps both the entry count and per-field length to bound token cost.
pub fn build_proposal_prompt(entries: &[MemoryEntry]) -> String {
    let mut out = String::from("Recent activity log (newest first):\n\n");
    for entry in entries.iter().take(MAX_ENTRIES) {
        let date = entry.timestamp.format("%Y-%m-%d %H:%M");
        out.push_str(&format!(
            "- [{}] {}\n",
            date,
            truncate(&entry.summary, MAX_ENTRY_CHARS)
        ));
        if let Some(ctx) = &entry.context {
            let ctx = truncate(ctx, MAX_ENTRY_CHARS);
            if !ctx.trim().is_empty() {
                out.push_str("    ");
                out.push_str(&ctx.replace('\n', "\n    "));
                out.push('\n');
            }
        }
    }
    out.push_str("\nReturn the JSON array of durable facts now.");
    out
}

/// Private parse DTO — keeps `MemoryFact` serde-free (11.2 Q1).
#[derive(serde::Deserialize)]
struct ProposedFact {
    category: String,
    fact: String,
    #[serde(default)]
    detail: Option<String>,
}

/// Tolerantly parse a model reply into proposed `MemoryFact`s.
///
/// Strips surrounding prose / code fences by slicing the outermost JSON array,
/// then `serde_json`-parses into the private DTO. Skips blank items, trims, caps
/// the count, and NEVER panics — any malformed input yields `Vec::new()`.
pub fn parse_proposals(model_text: &str) -> Vec<MemoryFact> {
    let trimmed = model_text.trim();
    let start = match trimmed.find('[') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let end = match trimmed.rfind(']') {
        Some(i) => i,
        None => return Vec::new(),
    };
    if end < start {
        return Vec::new();
    }
    let json_slice = &trimmed[start..=end];
    let parsed: Vec<ProposedFact> = match serde_json::from_str(json_slice) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    parsed
        .into_iter()
        .filter_map(|p| {
            let category = p.category.trim().to_string();
            let fact = p.fact.trim().to_string();
            if category.is_empty() || fact.is_empty() {
                return None;
            }
            let detail = p.detail.and_then(|d| {
                let d = d.trim().to_string();
                if d.is_empty() { None } else { Some(d) }
            });
            Some(MemoryFact {
                category,
                fact,
                detail,
            })
        })
        .take(MAX_PROPOSALS)
        .collect()
}

/// Run the model sub-turn and parse its reply into proposed facts. Shared between
/// the TUI `/memory consolidate` handler and the daemon attach consolidation path
/// (Story 12.2d AC8 — extracted from `handlers/consolidation.rs`).
///
/// Applies the AC5 defense-in-depth secret gate (daily-log content can predate the
/// 11.2 capture gate) — any proposal whose text trips `scan_for_secrets` is dropped.
pub async fn generate_proposals(
    provider: &dyn crate::domain::ports::StreamingProvider,
    model: &str,
    prompt_body: &str,
) -> anyhow::Result<Vec<MemoryFact>> {
    use crate::domain::models::{CompletionOptions, Message, MessageRole};

    let messages = vec![Message {
        role: MessageRole::User,
        content: prompt_body.to_string(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
    }];
    let options = CompletionOptions {
        model: model.to_string(),
        max_tokens: 1024,
        system_prompt: CONSOLIDATION_SYSTEM_PROMPT.to_string(),
        temperature: None,
        tools: vec![], // MANDATORY — the model must emit JSON, not call tools.
    };

    let stream = provider.stream_completion(messages, options).await?;
    let text = crate::domain::services::streaming_collect::collect_text(stream).await?;
    let mut proposals = parse_proposals(&text);

    // AC5 — defense-in-depth secret gate at the propose boundary.
    proposals.retain(|f| {
        let blob = format!(
            "{}\n{}\n{}",
            f.category,
            f.fact,
            f.detail.as_deref().unwrap_or("")
        );
        match crate::domain::services::secret_scan::scan_for_secrets(&blob) {
            Some(pat) => {
                tracing::warn!("consolidation: dropping proposal flagged as {pat}");
                false
            }
            None => true,
        }
    });

    Ok(proposals)
}

/// Char-safe truncation with an ellipsis marker.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn entry(summary: &str, context: Option<&str>) -> MemoryEntry {
        MemoryEntry {
            timestamp: Local::now(),
            summary: summary.to_string(),
            context: context.map(|c| c.to_string()),
        }
    }

    #[test]
    fn parses_well_formed_array() {
        let text =
            r#"[{"category":"Preferences","fact":"User prefers snake_case","detail":"in Rust"}]"#;
        let facts = parse_proposals(text);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].category, "Preferences");
        assert_eq!(facts[0].fact, "User prefers snake_case");
        assert_eq!(facts[0].detail.as_deref(), Some("in Rust"));
    }

    #[test]
    fn parses_fenced_json_block() {
        let text = "```json\n[{\"category\":\"Build\",\"fact\":\"Use cargo nextest\"}]\n```";
        let facts = parse_proposals(text);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].category, "Build");
        assert_eq!(facts[0].detail, None);
    }

    #[test]
    fn parses_prose_wrapped_array() {
        let text = "Here are the durable facts I found:\n[{\"category\":\"DB\",\"fact\":\"Postgres 15\"}]\nThat's all.";
        let facts = parse_proposals(text);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].fact, "Postgres 15");
    }

    #[test]
    fn empty_array_yields_no_facts() {
        assert!(parse_proposals("[]").is_empty());
        assert!(parse_proposals("Nothing worth promoting.\n[]").is_empty());
    }

    #[test]
    fn garbage_never_panics_and_yields_empty() {
        assert!(parse_proposals("").is_empty());
        assert!(parse_proposals("not json at all").is_empty());
        assert!(parse_proposals("{not an array}").is_empty());
        assert!(parse_proposals("[ broken json").is_empty());
        assert!(parse_proposals("]premature").is_empty());
    }

    #[test]
    fn skips_blank_items_and_trims() {
        let text = r#"[{"category":"  ","fact":"x"},{"category":"Good","fact":"  "},{"category":" Topic ","fact":" the fact ","detail":"  "}]"#;
        let facts = parse_proposals(text);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].category, "Topic");
        assert_eq!(facts[0].fact, "the fact");
        assert_eq!(facts[0].detail, None);
    }

    #[test]
    fn caps_proposal_count() {
        let items: Vec<String> = (0..50)
            .map(|i| format!("{{\"category\":\"C{i}\",\"fact\":\"f{i}\"}}"))
            .collect();
        let text = format!("[{}]", items.join(","));
        let facts = parse_proposals(&text);
        assert_eq!(facts.len(), MAX_PROPOSALS);
    }

    #[test]
    fn missing_detail_maps_to_none() {
        let facts = parse_proposals(r#"[{"category":"C","fact":"f"}]"#);
        assert_eq!(facts[0].detail, None);
    }

    #[test]
    fn build_prompt_includes_summaries_and_system_prompt_demands_json() {
        let entries = vec![
            entry("Refactored the event loop", Some("split into handlers")),
            entry("Chose ratatui for the TUI", None),
        ];
        let prompt = build_proposal_prompt(&entries);
        assert!(prompt.contains("Refactored the event loop"));
        assert!(prompt.contains("Chose ratatui for the TUI"));
        assert!(prompt.contains("split into handlers"));
        // The system prompt is the JSON contract.
        assert!(CONSOLIDATION_SYSTEM_PROMPT.contains("JSON array"));
        assert!(CONSOLIDATION_SYSTEM_PROMPT.contains("[]"));
    }

    #[test]
    fn build_prompt_caps_entry_count_and_length() {
        let long = "x".repeat(MAX_ENTRY_CHARS + 100);
        let entries: Vec<MemoryEntry> = (0..MAX_ENTRIES + 10)
            .map(|i| entry(&format!("entry {i} {long}"), None))
            .collect();
        let prompt = build_proposal_prompt(&entries);
        // Only MAX_ENTRIES bullet lines (each starts with "- [").
        assert_eq!(prompt.matches("- [").count(), MAX_ENTRIES);
        // The ellipsis marker proves per-field truncation kicked in.
        assert!(prompt.contains('…'));
    }
}
