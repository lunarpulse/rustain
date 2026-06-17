//! `session list` handler (Story 13.5a).
//!
//! Consumes the shared `build_session_rows` core and renders either a human
//! table or a versioned snake_case JSON envelope.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Serialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::adapters::cli::session::rows::{
    SESSION_LIST_SCHEMA_VERSION, SessionRow, build_session_rows,
};
use crate::domain::ports::StoragePort;

/// Run `rustain session list`.
///
/// # Errors
///
/// Returns `Err` only for storage read errors or JSON serialization failures.
/// Never builds a provider and never performs a network call (AC5).
pub async fn run_session_list(json: bool, storage: &Arc<dyn StoragePort>) -> Result<()> {
    let summaries = storage.list_conversations_read_only().await?;
    let rows = build_session_rows(summaries);

    if json {
        println!("{}", render_json(&rows)?);
    } else if rows.is_empty() {
        println!("No saved sessions in this workspace yet.");
        println!();
        println!(
            "Start a conversation with `rustain` (or `rustain ask \"…\"`) and it'll show up here."
        );
    } else {
        println!("{}", render_human(&rows));
    }

    tracing::info!(subcommand = "session-list", sessions = rows.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// Render the rows as an aligned table with the `*` marker fused to the index
/// gutter. Column order: `#  ID  TITLE  LAST ACTIVITY  MESSAGES`.
fn render_human(rows: &[SessionRow]) -> String {
    let idx_header = "#";
    let id_header = "ID";
    let title_header = "TITLE";
    let activity_header = "LAST ACTIVITY";
    let messages_header = "MESSAGES";

    // The index gutter reserves one slot for the marker (`*` or space) plus
    // the width of the largest index number, so the table stays straight.
    let max_index = rows.len();
    let index_body_width = max_index.to_string().len();
    let index_column_width = 1 + index_body_width;

    let id_width = rows
        .iter()
        .map(|r| r.id.width())
        .max()
        .unwrap_or(0)
        .max(id_header.width());

    const TITLE_MAX_WIDTH: usize = 40;
    let title_width = rows
        .iter()
        .map(|r| display_width(&sanitize_title(&r.title)))
        .max()
        .unwrap_or(0)
        .min(TITLE_MAX_WIDTH)
        .max(title_header.width());

    let activity_width = activity_header.width();
    let messages_width = messages_header.width().max("0".len());

    let mut out = String::new();
    push_left_aligned(&mut out, idx_header, index_column_width);
    out.push_str("  ");
    push_left_aligned(&mut out, id_header, id_width);
    out.push_str("  ");
    push_left_aligned(&mut out, title_header, title_width);
    out.push_str("  ");
    push_left_aligned(&mut out, activity_header, activity_width);
    out.push_str("  ");
    push_right_aligned(&mut out, messages_header, messages_width);
    out.push('\n');

    for row in rows {
        let marker = if row.is_default_resume { '*' } else { ' ' };
        let idx = format!("{}{}", marker, row.index);
        let title = sanitize_title(&row.title);
        let truncated = truncate_str(&title, TITLE_MAX_WIDTH);
        let activity = format_timestamp(row.updated_at);
        let message_count = row.message_count.to_string();

        push_left_aligned(&mut out, &idx, index_column_width);
        out.push_str("  ");
        push_left_aligned(&mut out, &row.id, id_width);
        out.push_str("  ");
        push_left_aligned(&mut out, &truncated, title_width);
        out.push_str("  ");
        push_left_aligned(&mut out, &activity, activity_width);
        out.push_str("  ");
        push_right_aligned(&mut out, &message_count, messages_width);
        out.push('\n');
    }

    out.push_str("\n* = resumes by default (most recent)");
    out
}

fn push_left_aligned(out: &mut String, value: &str, width: usize) {
    out.push_str(value);
    push_spaces(out, width.saturating_sub(display_width(value)));
}

fn push_right_aligned(out: &mut String, value: &str, width: usize) {
    push_spaces(out, width.saturating_sub(display_width(value)));
    out.push_str(value);
}

fn push_spaces(out: &mut String, count: usize) {
    for _ in 0..count {
        out.push(' ');
    }
}

/// Format a unix timestamp as absolute local `YYYY-MM-DD HH:MM`.
fn format_timestamp(ts: i64) -> String {
    let dt = DateTime::from_timestamp(ts, 0).unwrap_or(DateTime::UNIX_EPOCH);
    dt.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Replace control characters with a single space and strip ANSI escape
/// sequences so titles cannot shatter the table or inject terminal controls.
fn sanitize_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut chars = title.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => skip_ansi_escape(&mut chars),
            ch if ch.is_control() => out.push(' '),
            _ => out.push(ch),
        }
    }

    out
}

fn skip_ansi_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    if chars.peek() == Some(&'[') {
        chars.next();
        while let Some(ch) = chars.next() {
            if ('\x40'..='\x7e').contains(&ch) {
                break;
            }
        }
    } else {
        let _ = chars.next();
    }
}

/// Display width of a string, clamped to `usize` range.
fn display_width(s: &str) -> usize {
    s.width()
}

/// Truncate `s` to at most `max_width` display columns, appending `…` when
/// truncated. Never byte-slices; works on `char` boundaries.
fn truncate_str(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if width + w + 1 > max_width {
            out.push('…');
            break;
        }
        out.push(ch);
        width += w;
    }
    out
}

// ---------------------------------------------------------------------------
// JSON rendering
// ---------------------------------------------------------------------------

fn render_json(rows: &[SessionRow]) -> Result<String> {
    let output = SessionListJson {
        schema_version: SESSION_LIST_SCHEMA_VERSION,
        sessions: rows.iter().map(SessionRowJson::from).collect(),
    };
    Ok(serde_json::to_string_pretty(&output)?)
}

#[derive(Serialize)]
struct SessionListJson<'a> {
    schema_version: &'a str,
    sessions: Vec<SessionRowJson>,
}

#[derive(Serialize)]
struct SessionRowJson {
    id: String,
    index: usize,
    title: String,
    message_count: usize,
    created_at: i64,
    updated_at: i64,
    has_fork_source: bool,
    is_default_resume: bool,
}

impl From<&SessionRow> for SessionRowJson {
    fn from(row: &SessionRow) -> Self {
        Self {
            id: row.id.clone(),
            index: row.index,
            title: row.title.clone(),
            message_count: row.message_count,
            created_at: row.created_at,
            updated_at: row.updated_at,
            has_fork_source: row.has_fork_source,
            is_default_resume: row.is_default_resume,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::ConversationSummary;

    fn row(id: &str, title: &str, updated_at: i64, message_count: usize) -> SessionRow {
        SessionRow {
            index: 1,
            id: id.to_string(),
            title: title.to_string(),
            message_count,
            created_at: updated_at,
            updated_at,
            has_fork_source: false,
            is_default_resume: true,
        }
    }

    #[test]
    fn p0_8_title_control_char_sanitization() {
        let rows = vec![
            row("a", "line\nbreak", 100, 1),
            row("b", "tab\there", 100, 1),
            row("c", "\x1b[31mred\x1b[0m", 100, 1),
        ];
        let rendered = render_human(&rows);
        // The table itself contains row-separator newlines; assert that no
        // *individual line* carries a raw control char or ANSI escape from a title.
        for line in rendered.lines() {
            assert!(
                !line.contains('\n'),
                "no embedded newlines in a rendered line"
            );
            assert!(!line.contains('\x1b'), "no ANSI escapes in a rendered line");
        }
        assert!(rendered.contains("line break"));
        assert!(rendered.contains("tab here"));
        assert!(rendered.contains("red"));
        assert!(!rendered.contains("[31m"));
        assert!(!rendered.contains("[0m"));
    }

    #[test]
    fn p0_9_unicode_width_alignment() {
        let rows = vec![
            row("a", "emoji 🚀 rocket 中文 α̂", 200, 1),
            row("b", "Plain", 100, 1),
        ];
        let rendered = render_human(&rows);
        // Most important: no panic on emoji/CJK/combining accent.
        assert!(rendered.contains('🚀'));
        assert!(rendered.contains("中文"));

        let activity_columns: Vec<usize> = rendered
            .lines()
            .filter_map(|line| {
                line.find("1970-").map(|idx| {
                    display_width(
                        line.get(..idx)
                            .expect("date marker is always on a char boundary"),
                    )
                })
            })
            .collect();
        assert_eq!(activity_columns.len(), 2);
        assert_eq!(activity_columns[0], activity_columns[1]);
    }

    #[test]
    fn p0_7_json_shape_and_no_secrets() {
        let rows = vec![
            SessionRow {
                index: 1,
                id: "sess-1".to_string(),
                title: "Hello".to_string(),
                message_count: 5,
                created_at: 10,
                updated_at: 100,
                has_fork_source: true,
                is_default_resume: true,
            },
            SessionRow {
                index: 2,
                id: "sess-2".to_string(),
                title: "World".to_string(),
                message_count: 1,
                created_at: 20,
                updated_at: 50,
                has_fork_source: false,
                is_default_resume: false,
            },
        ];
        let json = render_json(&rows).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.0");
        let sessions = parsed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);

        let first = &sessions[0];
        assert_eq!(first["id"].as_str().unwrap(), "sess-1");
        assert_eq!(first["index"].as_u64().unwrap(), 1);
        assert_eq!(first["title"].as_str().unwrap(), "Hello");
        assert_eq!(first["message_count"].as_u64().unwrap(), 5);
        assert_eq!(first["created_at"].as_i64().unwrap(), 10);
        assert_eq!(first["updated_at"].as_i64().unwrap(), 100);
        assert!(first["has_fork_source"].as_bool().unwrap());
        assert!(first["is_default_resume"].as_bool().unwrap());

        for s in sessions {
            assert!(s.get("url").is_none());
            assert!(s.get("base_url").is_none());
            assert!(s.get("key").is_none());
            assert!(s.get("token").is_none());
        }
    }

    #[test]
    fn p0_13_empty_json_envelope() {
        let json = render_json(&[]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.0");
        let sessions = parsed["sessions"].as_array().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn truncate_str_does_not_byte_slice() {
        let s = "中文标题";
        assert_eq!(truncate_str(s, 3), "中…");
        assert_eq!(truncate_str(s, 10), s);
    }
}
