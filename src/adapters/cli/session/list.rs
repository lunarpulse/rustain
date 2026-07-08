//! `session list` handler (Story 13.5a / 13.5a-1).
//!
//! Consumes the shared `build_session_rows` core and renders either a human
//! table or a versioned snake_case JSON envelope.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Serialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::adapters::cli::session::rows::{
    SESSION_LIST_SCHEMA_VERSION, WorkspaceSessionRow, build_all_workspace_rows, build_session_rows,
};
use crate::adapters::filesystem::FileSystemStorage;
use crate::domain::ports::{StoragePort, WorkspaceRegistryReaderPort};

/// Run `rustain session list`.
///
/// # Errors
///
/// Returns `Err` only for current-workspace storage read errors or JSON
/// serialization failures. Never builds a provider and never performs a
/// network call.
pub async fn run_session_list(
    json: bool,
    all: bool,
    current_workspace: &Path,
    current_storage: &Arc<dyn StoragePort>,
    reader: &Arc<dyn WorkspaceRegistryReaderPort>,
) -> Result<()> {
    let current_workspace = canonical_workspace_path(current_workspace);
    let current_workspace_str = current_workspace.to_string_lossy().to_string();
    let current_rows = current_storage.list_conversations_read_only().await?;

    let rows = if all {
        let mut per_workspace = vec![(current_workspace_str.clone(), current_rows)];
        let live_workspaces = match reader.live_workspaces().await {
            Ok(workspaces) => workspaces,
            Err(err) => {
                tracing::warn!("workspace registry unavailable during session list --all: {err}");
                vec![]
            }
        };

        for workspace in live_workspaces {
            let normalized = canonical_workspace_path(&workspace.path);
            if normalized == current_workspace {
                continue;
            }

            let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(&normalized),
                normalized.clone(),
            ));
            match storage.list_conversations_read_only().await {
                Ok(summaries) => {
                    per_workspace.push((normalized.to_string_lossy().to_string(), summaries));
                }
                Err(err) => {
                    tracing::warn!(
                        workspace = %normalized.display(),
                        "skipping workspace during session list --all: {err}"
                    );
                }
            }
        }

        build_all_workspace_rows(&current_workspace_str, per_workspace)
    } else {
        build_session_rows(current_rows)
            .into_iter()
            .map(|row| WorkspaceSessionRow {
                workspace: current_workspace_str.clone(),
                row,
            })
            .collect()
    };

    if json {
        println!("{}", render_json(&rows)?);
    } else if rows.is_empty() {
        if all {
            println!("No saved sessions found across registered workspaces yet.");
            println!();
            println!(
                "Start a conversation with `rustain` (or `rustain ask \"…\"`) and it'll show up here after the first save."
            );
        } else {
            println!("No saved sessions in this workspace yet.");
            println!();
            println!(
                "Start a conversation with `rustain` (or `rustain ask \"…\"`) and it'll show up here."
            );
        }
    } else {
        println!("{}", render_human(&rows, all, &current_workspace_str));
    }

    tracing::info!(subcommand = "session-list", all, sessions = rows.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// Render the rows as an aligned table. Under `--all`, the marker semantics stay
/// single-promise: `*` marks only the current workspace's default resume row.
fn render_human(
    rows: &[WorkspaceSessionRow],
    show_workspace: bool,
    current_workspace: &str,
) -> String {
    let idx_header = "#";
    let workspace_header = "WORKSPACE";
    let id_header = "ID";
    let title_header = "TITLE";
    let activity_header = "LAST ACTIVITY";
    let messages_header = "MESSAGES";

    let max_index = rows.len();
    let index_body_width = max_index.to_string().len();
    let index_column_width = 1 + index_body_width;

    let workspace_labels = if show_workspace {
        build_workspace_labels(rows)
    } else {
        vec![String::new(); rows.len()]
    };

    const WORKSPACE_MAX_WIDTH: usize = 24;
    const TITLE_MAX_WIDTH: usize = 40;

    let workspace_width = if show_workspace {
        workspace_labels
            .iter()
            .map(|label| display_width(label))
            .max()
            .unwrap_or(0)
            .max(workspace_header.width())
            .min(WORKSPACE_MAX_WIDTH)
    } else {
        0
    };

    let id_width = rows
        .iter()
        .map(|r| display_width(&r.row.id))
        .max()
        .unwrap_or(0)
        .max(id_header.width());

    let title_width = rows
        .iter()
        .map(|r| display_width(&sanitize_title(&r.row.title)))
        .max()
        .unwrap_or(0)
        .min(TITLE_MAX_WIDTH)
        .max(title_header.width());

    let activity_width = activity_header.width();
    let messages_width = messages_header.width().max("0".len());

    let mut out = String::new();
    push_left_aligned(&mut out, idx_header, index_column_width);
    out.push_str("  ");
    if show_workspace {
        push_left_aligned(&mut out, workspace_header, workspace_width);
        out.push_str("  ");
    }
    push_left_aligned(&mut out, id_header, id_width);
    out.push_str("  ");
    push_left_aligned(&mut out, title_header, title_width);
    out.push_str("  ");
    push_left_aligned(&mut out, activity_header, activity_width);
    out.push_str("  ");
    push_right_aligned(&mut out, messages_header, messages_width);
    out.push('\n');

    for (row, workspace_label) in rows.iter().zip(workspace_labels.iter()) {
        let marker = if row.row.is_default_resume { '*' } else { ' ' };
        let idx = format!("{}{}", marker, row.row.index);
        let title = sanitize_title(&row.row.title);
        let truncated_title = truncate_end(&title, TITLE_MAX_WIDTH);
        let activity = format_timestamp(row.row.updated_at);
        let message_count = row.row.message_count.to_string();

        push_left_aligned(&mut out, &idx, index_column_width);
        out.push_str("  ");
        if show_workspace {
            push_left_aligned(
                &mut out,
                &truncate_middle(workspace_label, WORKSPACE_MAX_WIDTH),
                workspace_width,
            );
            out.push_str("  ");
        }
        push_left_aligned(&mut out, &row.row.id, id_width);
        out.push_str("  ");
        push_left_aligned(&mut out, &truncated_title, title_width);
        out.push_str("  ");
        push_left_aligned(&mut out, &activity, activity_width);
        out.push_str("  ");
        push_right_aligned(&mut out, &message_count, messages_width);
        out.push('\n');
    }

    if show_workspace {
        if rows.iter().any(|row| row.row.is_default_resume) {
            out.push_str("\n* = resumes by default (most recent in current workspace ");
            out.push_str(&abbreviate_home(current_workspace));
            out.push(')');
        } else {
            out.push_str(
                "\nno * shown — bare `rustain` won't resume from here (cd into a workspace below)",
            );
        }
    } else {
        out.push_str("\n* = resumes by default (most recent)");
    }
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

fn build_workspace_labels(rows: &[WorkspaceSessionRow]) -> Vec<String> {
    let parts: Vec<Vec<String>> = rows
        .iter()
        .map(|row| workspace_components(&row.workspace))
        .collect();
    let mut depths = vec![1usize; rows.len()];

    loop {
        let labels: Vec<String> = parts
            .iter()
            .zip(depths.iter())
            .map(|(parts, depth)| suffix_label(parts, *depth))
            .collect();
        let mut duplicates = HashMap::<String, Vec<usize>>::new();
        for (idx, label) in labels.iter().enumerate() {
            duplicates.entry(label.clone()).or_default().push(idx);
        }

        let mut changed = false;
        for indices in duplicates.values() {
            if indices.len() < 2 {
                continue;
            }
            for &idx in indices {
                if depths[idx] < parts[idx].len() {
                    depths[idx] += 1;
                    changed = true;
                }
            }
        }

        if !changed {
            return labels;
        }
    }
}

fn workspace_components(path: &str) -> Vec<String> {
    let mut parts: Vec<String> = Path::new(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        parts.push(path.to_string());
    }
    parts
}

fn suffix_label(parts: &[String], depth: usize) -> String {
    let depth = depth.min(parts.len()).max(1);
    parts[parts.len() - depth..].join("/")
}

/// Format a unix timestamp as absolute local `YYYY-MM-DD HH:MM`.
fn format_timestamp(ts: i64) -> String {
    let dt = DateTime::from_timestamp(ts, 0).unwrap_or(DateTime::UNIX_EPOCH);
    dt.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Replace control characters with a single space and strip ANSI escape
/// sequences (CSI and OSC) so titles cannot shatter the table or inject
/// terminal controls. Shared by `session list` and `session delete`.
pub(crate) fn sanitize_title(title: &str) -> String {
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
    match chars.peek() {
        // CSI: ESC [ <params> <0x40..=0x7E final>.
        Some('[') => {
            chars.next();
            for ch in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&ch) {
                    break;
                }
            }
        }
        // OSC: ESC ] <payload> BEL (\x07) or ST (ESC \). Strips terminal-title
        // injections like `\x1b]0;evil\x07` instead of leaking the payload.
        Some(']') => {
            chars.next();
            let mut prev = None;
            for ch in chars.by_ref() {
                if ch == '\x07' {
                    break;
                }
                if prev == Some('\x1b') && ch == '\\' {
                    break;
                }
                prev = Some(ch);
            }
        }
        // Other Fe introducers (DCS/SOS/PM/APC): drop ESC + the introducer byte.
        Some(_) => {
            let _ = chars.next();
        }
        None => {}
    }
}

/// Display width of a string, clamped to `usize` range.
fn display_width(s: &str) -> usize {
    s.width()
}

fn truncate_end(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
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

fn truncate_middle(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }

    let head_target = (max_width - 1) / 2;
    let tail_target = max_width - 1 - head_target;
    let head = take_prefix_width(s, head_target);
    let tail = take_suffix_width(s, tail_target);
    format!("{head}…{tail}")
}

fn take_prefix_width(s: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if width + w > max_width {
            break;
        }
        out.push(ch);
        width += w;
    }
    out
}

fn take_suffix_width(s: &str, max_width: usize) -> String {
    let mut out = Vec::new();
    let mut width = 0;
    for ch in s.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if width + w > max_width {
            break;
        }
        out.push(ch);
        width += w;
    }
    out.into_iter().rev().collect()
}

fn canonical_workspace_path(workspace: &Path) -> PathBuf {
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(workspace)
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

fn abbreviate_home(abs: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return abs.to_string();
    };
    let home = home.to_string_lossy();
    if abs == home {
        return "~".to_string();
    }
    if let Some(stripped) = abs.strip_prefix(home.as_ref()) {
        if stripped.starts_with(std::path::is_separator) {
            return format!("~{stripped}");
        }
    }
    abs.to_string()
}

// ---------------------------------------------------------------------------
// JSON rendering
// ---------------------------------------------------------------------------

fn render_json(rows: &[WorkspaceSessionRow]) -> Result<String> {
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
    workspace: String,
}

impl From<&WorkspaceSessionRow> for SessionRowJson {
    fn from(row: &WorkspaceSessionRow) -> Self {
        Self {
            id: row.row.id.clone(),
            index: row.row.index,
            title: row.row.title.clone(),
            message_count: row.row.message_count,
            created_at: row.row.created_at,
            updated_at: row.row.updated_at,
            has_fork_source: row.row.has_fork_source,
            is_default_resume: row.row.is_default_resume,
            workspace: row.workspace.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        workspace: &str,
        id: &str,
        title: &str,
        updated_at: i64,
        message_count: usize,
    ) -> WorkspaceSessionRow {
        WorkspaceSessionRow {
            workspace: workspace.to_string(),
            row: crate::adapters::cli::session::rows::SessionRow {
                index: 1,
                id: id.to_string(),
                title: title.to_string(),
                message_count,
                created_at: updated_at,
                updated_at,
                has_fork_source: false,
                is_default_resume: true,
            },
        }
    }

    #[test]
    fn p0_8_title_control_char_sanitization() {
        let rows = vec![
            row("/ws/a", "a", "line\nbreak", 100, 1),
            row("/ws/b", "b", "tab\there", 100, 1),
            row("/ws/c", "c", "\x1b[31mred\x1b[0m", 100, 1),
        ];
        let rendered = render_human(&rows, false, "/ws/a");
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
            row("/ws/a", "a", "emoji 🚀 rocket 中文 α̂", 200, 1),
            row("/ws/b", "b", "Plain", 100, 1),
        ];
        let rendered = render_human(&rows, false, "/ws/a");
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
            row("/workspace/one", "sess-1", "Hello", 100, 5),
            WorkspaceSessionRow {
                workspace: "/workspace/two".to_string(),
                row: crate::adapters::cli::session::rows::SessionRow {
                    index: 2,
                    id: "sess-2".to_string(),
                    title: "World".to_string(),
                    message_count: 1,
                    created_at: 20,
                    updated_at: 50,
                    has_fork_source: false,
                    is_default_resume: false,
                },
            },
        ];
        let json = render_json(&rows).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.1");
        let sessions = parsed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);

        let first = &sessions[0];
        assert_eq!(first["id"].as_str().unwrap(), "sess-1");
        assert_eq!(first["index"].as_u64().unwrap(), 1);
        assert_eq!(first["title"].as_str().unwrap(), "Hello");
        assert_eq!(first["message_count"].as_u64().unwrap(), 5);
        assert_eq!(first["created_at"].as_i64().unwrap(), 100);
        assert_eq!(first["updated_at"].as_i64().unwrap(), 100);
        assert!(!first["has_fork_source"].as_bool().unwrap());
        assert!(first["is_default_resume"].as_bool().unwrap());
        assert_eq!(first["workspace"].as_str().unwrap(), "/workspace/one");

        for s in sessions {
            assert!(s.get("url").is_none());
            assert!(s.get("base_url").is_none());
            assert!(s.get("key").is_none());
            assert!(s.get("token").is_none());
            assert!(Path::new(s["workspace"].as_str().unwrap()).is_absolute());
        }
    }

    #[test]
    fn p0_13_empty_json_envelope() {
        let json = render_json(&[]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"].as_str().unwrap(), "1.1");
        let sessions = parsed["sessions"].as_array().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn p0_10_workspace_collision_promotes_parent_segments() {
        let rows = vec![
            row("/home/me/work/api", "a", "Alpha", 200, 1),
            row("/home/me/play/api", "b", "Beta", 100, 1),
        ];
        let rendered = render_human(&rows, true, "/home/me/work/api");
        assert!(rendered.contains("work/api"));
        assert!(rendered.contains("play/api"));
        assert!(rendered.contains("WORKSPACE"));
    }

    #[test]
    fn p0_10_workspace_uses_leaf_name_without_collision() {
        let rows = vec![
            row("/home/me/work/api", "a", "Alpha", 200, 1),
            row("/home/me/other/docs", "b", "Beta", 100, 1),
        ];
        let rendered = render_human(&rows, true, "/home/me/work/api");
        assert!(rendered.contains("api"));
        assert!(rendered.contains("docs"));
    }

    #[test]
    fn p0_3_all_mode_zero_marker_footer_when_current_workspace_missing() {
        let rows = vec![
            WorkspaceSessionRow {
                workspace: "/home/me/work/api".to_string(),
                row: crate::adapters::cli::session::rows::SessionRow {
                    index: 1,
                    id: "a".to_string(),
                    title: "Alpha".to_string(),
                    message_count: 1,
                    created_at: 100,
                    updated_at: 100,
                    has_fork_source: false,
                    is_default_resume: false,
                },
            },
            WorkspaceSessionRow {
                workspace: "/home/me/other/docs".to_string(),
                row: crate::adapters::cli::session::rows::SessionRow {
                    index: 2,
                    id: "b".to_string(),
                    title: "Beta".to_string(),
                    message_count: 1,
                    created_at: 50,
                    updated_at: 50,
                    has_fork_source: false,
                    is_default_resume: false,
                },
            },
        ];
        let rendered = render_human(&rows, true, "/tmp/outside");
        assert!(rendered.contains("no * shown"));
    }

    #[test]
    fn p0_9_human_legend_abbreviates_home_only() {
        let current = dirs::home_dir().unwrap().join("project");
        let current_str = current.to_string_lossy().to_string();
        let rows = vec![row(&current_str, "a", "Alpha", 200, 1)];
        let rendered = render_human(&rows, true, &current_str);
        assert!(rendered.contains("~/project"));
        assert!(!rendered.contains(&current_str));
    }

    #[test]
    fn truncate_helpers_do_not_byte_slice() {
        let s = "中文标题";
        assert_eq!(truncate_end(s, 3), "中…");
        assert_eq!(truncate_middle(s, 3), "…");
        assert_eq!(truncate_middle(s, 5), "中…题");
        assert_eq!(truncate_end(s, 10), s);
    }
}
