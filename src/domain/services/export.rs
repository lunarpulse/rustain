//! Pure domain function for rendering a `Conversation` as a markdown
//! document (Story 4-4 AC11).
//!
//! Zero I/O — returns a `String` that the event-loop layer writes to disk
//! atomically. Deterministic for a given `(conversation, meta, exported_at)`
//! so roundtrip tests are stable.
//!
//! Tool calls render as `<details>` / `<summary>` collapsed blocks — the
//! reader sees the tool name and can expand to view input / result. Inline
//! diffs for write-type tool calls are deferred to Story 4-6 cleanup
//! (DF-004); the current v1 is plain-text-only and forward-compatible.

use crate::domain::models::ContentBlockType;
use crate::domain::models::MessageRole;
use crate::domain::models::SessionMeta;
use crate::domain::models::conversation::Conversation;
use crate::domain::models::plan::PlanStatus;

/// Render a conversation as a markdown document. See AC11 for format.
///
/// `exported_at` is an explicit parameter so tests can pin timestamps.
pub fn render_conversation_markdown(
    conv: &Conversation,
    meta: &SessionMeta,
    exported_at: i64,
) -> String {
    let mut out =
        String::with_capacity(conv.messages.iter().map(|m| m.content.len()).sum::<usize>() + 1024);

    let title = if conv.title.is_empty() {
        "(untitled)"
    } else {
        conv.title.as_str()
    };
    out.push_str("# ");
    out.push_str(title);
    out.push_str("\n\n");

    out.push_str(&format!("**Conversation ID:** {}\n", conv.id));
    out.push_str(&format!("**Created:** {}\n", iso8601(conv.created_at)));
    out.push_str(&format!("**Updated:** {}\n", iso8601(conv.updated_at)));
    out.push_str(&format!("**Messages:** {}\n", meta.message_count));
    out.push_str(&format!("**Exported:** {}\n", iso8601(exported_at)));
    out.push_str("\n---\n\n");

    for msg in &conv.messages {
        let role = match msg.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
        };
        out.push_str(&format!("## {} ({})\n\n", role, iso8601(msg.created_at)));

        // Text content — preserve verbatim.
        if !msg.content.is_empty() {
            out.push_str(&msg.content);
            if !msg.content.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }

        // Tool calls — render as <details> collapsed blocks.
        for tc in &msg.tool_calls {
            out.push_str(&format!("### Tool: {} ({})\n\n", tc.name, tc.id));
            out.push_str("<details>\n<summary>Tool call</summary>\n\n");
            out.push_str("**Input:**\n```json\n");
            match serde_json::to_string_pretty(&tc.input) {
                Ok(s) => out.push_str(&s),
                Err(_) => out.push_str("(unrenderable input)"),
            }
            out.push_str("\n```\n\n");
            out.push_str("**Result:**\n```\n");
            let result_text = tc
                .result
                .as_ref()
                .map(|r| r.content.as_str())
                .unwrap_or("(no result)");
            out.push_str(result_text);
            out.push_str("\n```\n\n");
            out.push_str("</details>\n\n");
        }

        // Plan card — render plan details if this message contains a PlanCard block.
        if msg
            .content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlockType::PlanCard))
        {
            let matching_plans: Vec<_> = conv
                .plans
                .values()
                .filter(|p| p.host_message_id.as_deref() == Some(msg.id.as_str()))
                .collect();
            if matching_plans.is_empty() {
                out.push_str("*(Plan data missing or corrupted)*\n\n");
            }
            for plan in matching_plans {
                out.push_str(&format!("### Plan: {}\n\n", plan.title));

                let status_str = match plan.status {
                    PlanStatus::Pending => "Pending",
                    PlanStatus::Executing => "Executing",
                    PlanStatus::Completed => "Completed",
                    PlanStatus::Rejected => "Rejected",
                    PlanStatus::Editing => "Editing",
                    PlanStatus::Cancelled => "Cancelled",
                };

                if let Some(effort) = &plan.estimated_effort {
                    let mut parts = Vec::new();
                    if let Some(tc) = effort.tool_calls {
                        parts.push(format!("{} tool calls", tc));
                    }
                    if let Some(s) = effort.seconds {
                        parts.push(format!("~{}s", s));
                    }
                    if !parts.is_empty() {
                        out.push_str(&format!(
                            "> Estimated: {} · Status: {}\n\n",
                            parts.join(", "),
                            status_str
                        ));
                    } else {
                        out.push_str(&format!("> Status: {}\n\n", status_str));
                    }
                } else {
                    out.push_str(&format!("> Status: {}\n\n", status_str));
                }

                for task in &plan.tasks {
                    if task.description.is_empty() {
                        out.push_str(&format!("{}. **{}**\n", task.number, task.title));
                    } else {
                        out.push_str(&format!(
                            "{}. **{}** — {}\n",
                            task.number, task.title, task.description
                        ));
                    }
                    if !task.depends_on.is_empty() {
                        let deps: Vec<String> =
                            task.depends_on.iter().map(|d| d.to_string()).collect();
                        out.push_str(&format!("   - depends on: {}\n", deps.join(", ")));
                    }
                }
                out.push('\n');
            }
        }

        // Image attachments — v1 stubs per AC11 wording. The stored
        // `ImageReference` only carries filename/media-type/size metadata
        // (the raw bytes live in the sessions dir), so the stub encodes the
        // filename in the base64 slot to distinguish it from a real asset
        // reference while signaling to downstream consumers that the bytes
        // are not inlined here.
        for img in &msg.images {
            out.push_str(&format!("![image](base64:{}...)\n\n", img.file_name));
        }

        out.push_str("---\n\n");
    }

    out
}

/// Slugify a title into a safe filename component.
///
/// Lowercase, non-alphanumeric → `-`, collapse runs, trim, max 50 chars.
/// Empty or all-punctuation input returns `"conversation"`.
pub fn slugify(title: &str) -> String {
    let lowered = title.to_lowercase();
    let mut buf = String::with_capacity(lowered.len());
    let mut last_was_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_alphanumeric() {
            buf.push(c);
            last_was_dash = false;
        } else if !last_was_dash && !buf.is_empty() {
            buf.push('-');
            last_was_dash = true;
        }
    }
    // Trim trailing dashes.
    while buf.ends_with('-') {
        buf.pop();
    }
    // Clamp to 50 chars.
    if buf.chars().count() > 50 {
        buf = buf.chars().take(50).collect();
        while buf.ends_with('-') {
            buf.pop();
        }
    }
    if buf.is_empty() {
        "conversation".to_string()
    } else {
        buf
    }
}

/// Format a Unix seconds timestamp as an ISO-8601 UTC string (`YYYY-MM-DDTHH:MM:SSZ`).
///
/// Lightweight implementation — we don't need chrono just for this.
fn iso8601(unix_seconds: i64) -> String {
    // Unix epoch is 1970-01-01T00:00:00Z. Convert to y/m/d/h/m/s via manual arithmetic.
    let days_since_epoch = unix_seconds.div_euclid(86_400);
    let mut day_secs = unix_seconds.rem_euclid(86_400);
    let hour = (day_secs / 3600) as u32;
    day_secs %= 3600;
    let minute = (day_secs / 60) as u32;
    let second = (day_secs % 60) as u32;

    // Days → Gregorian date.
    let (year, month, day) = gregorian_from_days(days_since_epoch);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, minute, second
    )
}

/// Convert days-since-epoch to (year, month, day). Handles leap years.
fn gregorian_from_days(mut days: i64) -> (i32, u32, u32) {
    // Days from 1970-01-01
    let mut year: i32 = 1970;
    // Step year by year. Cheap for reasonable ranges; replace with a formula if
    // this becomes hot.
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }
    // Handle negative days (dates before 1970) — not used in practice but safe.
    while days < 0 {
        year -= 1;
        days += if is_leap(year) { 366 } else { 365 };
    }
    let month_lengths: [i64; 12] = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month: u32 = 0;
    for (i, &len) in month_lengths.iter().enumerate() {
        if days < len {
            month = i as u32 + 1;
            break;
        }
        days -= len;
    }
    let day = (days + 1) as u32;
    (year, month, day)
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::MessageRole;
    use crate::domain::models::SessionMeta;
    use crate::domain::models::conversation::{ChatMessage, Conversation};

    fn msg(role: MessageRole, content: &str, created_at: i64) -> ChatMessage {
        ChatMessage {
            id: "m".to_string(),
            role,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        }
    }

    fn conv(title: &str, messages: Vec<ChatMessage>) -> Conversation {
        Conversation {
            id: "conv-abc12345".to_string(),
            title: title.to_string(),
            messages,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_060,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        }
    }

    fn meta(message_count: usize) -> SessionMeta {
        SessionMeta {
            version: 1,
            title: "Test".to_string(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_060,
            message_count,
            bookmarks: vec![],
            fork_source: None,
            imported_from: None,
            plan_slug: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn empty_conversation_renders_frontmatter_only() {
        let c = conv("Empty", vec![]);
        let m = meta(0);
        let out = render_conversation_markdown(&c, &m, 1_700_000_123);
        assert!(out.starts_with("# Empty\n\n"));
        assert!(out.contains("**Conversation ID:** conv-abc12345"));
        assert!(out.contains("**Messages:** 0"));
        assert!(out.contains("**Exported:**"));
        assert!(out.contains("---"));
        // No message sections.
        assert!(!out.contains("## User"));
        assert!(!out.contains("## Assistant"));
    }

    #[test]
    fn single_user_assistant_pair_round_trips_content() {
        let c = conv(
            "Pair",
            vec![
                msg(MessageRole::User, "Hello world", 1_700_000_000),
                msg(
                    MessageRole::Assistant,
                    "Hi there! How can I help?",
                    1_700_000_010,
                ),
            ],
        );
        let m = meta(2);
        let out = render_conversation_markdown(&c, &m, 1_700_000_123);
        assert!(out.contains("## User"));
        assert!(out.contains("Hello world"));
        assert!(out.contains("## Assistant"));
        assert!(out.contains("Hi there! How can I help?"));
    }

    #[test]
    fn empty_title_renders_untitled() {
        let c = conv("", vec![]);
        let m = meta(0);
        let out = render_conversation_markdown(&c, &m, 1_700_000_000);
        assert!(out.starts_with("# (untitled)"));
    }

    #[test]
    fn iso8601_matches_known_dates() {
        // 1970-01-01T00:00:00Z
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        // 2023-11-14T22:13:20Z — 1700000000 seconds after epoch
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn slugify_basic_cases() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("  spaces  "), "spaces");
        assert_eq!(slugify("a/b/c"), "a-b-c");
        assert_eq!(slugify("UPPER_case"), "upper-case");
        assert_eq!(slugify(""), "conversation");
        assert_eq!(slugify("!!!"), "conversation");
        assert_eq!(slugify("---"), "conversation");
    }

    #[test]
    fn slugify_clamps_to_50_chars() {
        let long = "a".repeat(100);
        let slug = slugify(&long);
        assert!(slug.chars().count() <= 50);
    }

    #[test]
    fn slugify_utf8_input_produces_ascii_output() {
        assert_eq!(slugify("héllo wörld"), "h-llo-w-rld");
    }

    #[test]
    fn plan_card_renders_in_export() {
        use crate::domain::models::ContentBlockType;
        use crate::domain::models::plan::{
            EffortEstimate, Plan, PlanStatus, PlanTask, PlanTaskStatus,
        };

        let plan_msg = ChatMessage {
            id: "msg-plan-1".to_string(),
            role: MessageRole::Assistant,
            content: "Here is the plan.".to_string(),
            content_blocks: vec![ContentBlockType::PlanCard],
            tool_calls: vec![],
            created_at: 1_700_000_020,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        };

        let mut plans = std::collections::HashMap::new();
        plans.insert(
            "plan-1".to_string(),
            Plan {
                id: "plan-1".to_string(),
                title: "Refactor module".to_string(),
                tasks: vec![
                    PlanTask {
                        number: 1,
                        title: "Extract trait".to_string(),
                        description: "Move to separate file".to_string(),
                        depends_on: vec![],
                        status: PlanTaskStatus::Pending,
                        started_at_ms: None,
                        completed_at_ms: None,
                        result: None,
                        error: None,
                        waiting_on: vec![],
                    },
                    PlanTask {
                        number: 2,
                        title: "Update imports".to_string(),
                        description: String::new(),
                        depends_on: vec![1],
                        status: PlanTaskStatus::Pending,
                        started_at_ms: None,
                        completed_at_ms: None,
                        result: None,
                        error: None,
                        waiting_on: vec![],
                    },
                ],
                estimated_effort: Some(EffortEstimate {
                    tool_calls: Some(3),
                    seconds: Some(45),
                }),
                status: PlanStatus::Pending,
                created_at: 1_700_000_020,
                resolved_at: None,
                host_message_id: Some("msg-plan-1".to_string()),
            },
        );

        let mut c = conv(
            "PlanTest",
            vec![
                msg(MessageRole::User, "Do it", 1_700_000_000),
                plan_msg,
            ],
        );
        c.plans = plans;

        let m = meta(2);
        let out = render_conversation_markdown(&c, &m, 1_700_000_123);

        assert!(out.contains("### Plan: Refactor module"));
        assert!(out.contains("> Estimated: 3 tool calls, ~45s · Status: Pending"));
        assert!(out.contains("1. **Extract trait** — Move to separate file"));
        assert!(out.contains("2. **Update imports**"));
        assert!(out.contains("   - depends on: 1"));
    }
}
