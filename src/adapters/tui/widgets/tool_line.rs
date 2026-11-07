//! The humanized Stage tool line (Story 21.9 D12-D14).
//!
//! One row per tool call, read like a sentence:
//! `<lead><glyph> <Verb> <target>[ <detail>][ · <elapsed>]`. Every piece of
//! model-authored text is sanitized once (`sanitize_display_line`) and its
//! whitespace collapsed, so no control sequence reaches a cell and no newline
//! splits the row. Raw JSON is never shown for any tool.

use std::borrow::Cow;
use std::path::Path;

use ratatui::prelude::*;

use super::tool_block::display_tool_name;
use crate::adapters::daemon::protocol::ToolInputWithheld;
use crate::adapters::toolset_adapter::READ_DEFAULT_LIMIT;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::clock::Clock;
use crate::domain::models::{DiffKind, ToolCallInfo, WriteDiffState, diff_stat};
use crate::domain::services::peer_text::{sanitize_display_line, sanitize_tool_text_block};

/// Cells of the lead span (`┄ ` / `┃ `): the focus-marker contract.
const LEAD_CELLS: usize = 2;
/// The smallest target the fit keeps before the detail and then the verb give way.
const MIN_TARGET_CELLS: usize = 8;
const ELLIPSIS: &str = "…";

/// Top-level keys the generic summary prefers, in order.
const GENERIC_KEYS: &[&str] = &[
    "file_path",
    "path",
    "pattern",
    "query",
    "url",
    "command",
    "name",
    "skill",
    "description",
    "prompt",
    "text",
];

/// Where an over-long target loses its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// Paths keep their tail (`…/parser/mod.rs`).
    Left,
    /// Commands and prose keep their head.
    Right,
}

/// The change summary after the target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Detail {
    None,
    /// Exact `+added −removed` (U+2212).
    Stat {
        added: usize,
        removed: usize,
    },
    /// A new file: `+N` lines.
    Plus(usize),
    /// A line total where exact counts are unknowable (capped diff, no snapshot, running).
    Lines(usize),
    NoChange,
    Text(String),
}

impl Detail {
    fn text(&self) -> String {
        match self {
            Detail::None => String::new(),
            Detail::Stat { added, removed } => format!("+{added} \u{2212}{removed}"),
            Detail::Plus(n) => format!("+{n}"),
            Detail::Lines(1) => "1 line".to_owned(),
            Detail::Lines(n) => format!("{n} lines"),
            Detail::NoChange => "no change".to_owned(),
            Detail::Text(text) => text.clone(),
        }
    }

    fn spans(&self, text: String, theme: &Theme) -> Vec<Span<'static>> {
        let plain = Style::default().fg(theme.colors.fg_secondary);
        match self {
            Detail::Stat { added, removed } => vec![
                Span::styled(
                    format!("+{added}"),
                    Style::default().fg(theme.colors.success),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("\u{2212}{removed}"),
                    Style::default().fg(theme.colors.error),
                ),
            ],
            Detail::Plus(_) => vec![Span::styled(
                text,
                Style::default().fg(theme.colors.success),
            )],
            _ => vec![Span::styled(text, plain)],
        }
    }
}

struct Parts {
    target: String,
    cut: Cut,
    detail: Detail,
}

/// Sanitize once and collapse every whitespace run (newlines and tabs included).
fn clean(text: &str) -> String {
    sanitize_display_line(text)
}

/// A path under the workspace root displays relative to it; anything else as
/// given. The root is matched on the RAW path, component-wise: a path that only
/// becomes the root's child after control characters are stripped is a
/// different path and shows in full (never as a workspace-relative row).
fn path_text(raw: &str, root: Option<&Path>) -> String {
    if let Some(root) = root
        && let Ok(rel) = Path::new(raw).strip_prefix(root)
    {
        let rel = clean(&rel.to_string_lossy());
        return if rel.is_empty() { ".".to_owned() } else { rel };
    }
    clean(raw)
}

/// Whether the call has started or finished: a validating, scheduled,
/// awaiting-approval, cancelled or pending call changed nothing, so it shows
/// no change summary.
fn applied(tc: &ToolCallInfo) -> bool {
    match (&tc.result, tc.status.as_deref()) {
        (_, Some("⋯ Validating" | "⧖ Scheduled" | "? Awaiting approval" | "⊘ Cancelled")) => {
            false
        }
        (Some(_), _) | (None, Some(_)) => true,
        (None, None) => false,
    }
}

fn str_of<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(serde_json::Value::as_str)
}

/// Content lines of a Write input, when it carries `content`.
fn content_lines(input: &serde_json::Value) -> Option<usize> {
    str_of(input, "content").map(|content| content.lines().count())
}

fn write_detail(tc: &ToolCallInfo) -> Detail {
    if !applied(tc) {
        return Detail::None;
    }
    let lines = content_lines(&tc.input);
    let total = || lines.map_or(Detail::None, Detail::Lines);
    let Some(result) = &tc.result else {
        return total();
    };
    if result.is_error {
        return Detail::None;
    }
    match &result.diff {
        WriteDiffState::NewFile => lines.map_or(Detail::None, Detail::Plus),
        WriteDiffState::Diff {
            lines: rows,
            more: 0,
        } => {
            let (mut added, mut removed) = (0, 0);
            for row in rows {
                match row.kind {
                    DiffKind::Added => added += 1,
                    DiffKind::Removed => removed += 1,
                    DiffKind::Context | DiffKind::Elided => {}
                }
            }
            if rows.is_empty() {
                Detail::NoChange
            } else if added + removed == 0 {
                // Only an annotation row (line endings or a trailing newline
                // changed): bytes changed but no line did, so say how long it is.
                total()
            } else {
                Detail::Stat { added, removed }
            }
        }
        // A capped diff cannot give exact counts, and a missing snapshot gives
        // none at all: never a partial stat (PC-4).
        WriteDiffState::Diff { .. }
        | WriteDiffState::NotCaptured { .. }
        | WriteDiffState::NotAWrite => total(),
    }
}

fn edit_detail(tc: &ToolCallInfo) -> Detail {
    if !applied(tc) || tc.result.as_ref().is_some_and(|result| result.is_error) {
        return Detail::None;
    }
    match (
        str_of(&tc.input, "old_string"),
        str_of(&tc.input, "new_string"),
    ) {
        (Some(old), Some(new)) if old != new => match diff_stat(old, new) {
            Some(stat) if stat.added + stat.removed > 0 => Detail::Stat {
                added: stat.added,
                removed: stat.removed,
            },
            // Over the per-frame budget, or no changed line: no stat at all.
            _ => Detail::None,
        },
        _ => Detail::None,
    }
}

fn read_target(input: &serde_json::Value, root: Option<&Path>) -> String {
    let path = path_text(str_of(input, "file_path").unwrap_or(""), root);
    let offset = input.get("offset").and_then(serde_json::Value::as_u64);
    let limit = input.get("limit").and_then(serde_json::Value::as_u64);
    if offset.is_none() && limit.is_none() {
        return path;
    }
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(READ_DEFAULT_LIMIT as u64);
    format!(
        "{path}:{}-{}",
        offset.saturating_add(1),
        offset.saturating_add(limit)
    )
}

fn bash_target(input: &serde_json::Value) -> String {
    let command = sanitize_tool_text_block(str_of(input, "command").unwrap_or(""));
    let mut lines = command.trim().lines();
    let first = clean(lines.next().unwrap_or(""));
    let more = lines.any(|line| !line.trim().is_empty());
    if more {
        format!("{first} {ELLIPSIS}")
    } else {
        first
    }
}

fn patch_parts(input: &serde_json::Value, root: Option<&Path>) -> (String, Detail) {
    let patch = str_of(input, "patch").unwrap_or("");
    let mut seen: Vec<String> = Vec::new();
    for line in patch.lines() {
        let header = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix));
        if let Some(raw) = header {
            let shown = path_text(raw, root);
            // The executor folds repeated headers of one path onto one buffer.
            if !seen.contains(&shown) {
                seen.push(shown);
            }
        }
    }
    let Some((first, rest)) = seen.split_first() else {
        return (String::new(), Detail::None);
    };
    let detail = match rest.len() {
        0 => Detail::None,
        1 => Detail::Text("+1 file".to_owned()),
        k => Detail::Text(format!("+{k} files")),
    };
    (first.clone(), detail)
}

/// The first string value among the preferred keys, else the first top-level
/// string value, else nothing. Nested JSON is never shown.
fn generic_target(input: &serde_json::Value, root: Option<&Path>) -> (String, Cut) {
    let render = |key: &str, value: &str| {
        if matches!(key, "file_path" | "path") {
            (path_text(value, root), Cut::Left)
        } else {
            (clean(value), Cut::Right)
        }
    };
    let pick = |key: &str, value: &str| {
        let (text, cut) = render(key, value);
        (!text.is_empty()).then_some((text, cut))
    };
    for key in GENERIC_KEYS {
        if let Some(found) = str_of(input, key).and_then(|value| pick(key, value)) {
            return found;
        }
    }
    if let Some(map) = input.as_object() {
        for (key, value) in map {
            if let Some(found) = value.as_str().and_then(|value| pick(key, value)) {
                return found;
            }
        }
    }
    (String::new(), Cut::Right)
}

fn parts(tc: &ToolCallInfo, withheld: Option<&ToolInputWithheld>, root: Option<&Path>) -> Parts {
    if let Some(withheld) = withheld {
        return Parts {
            target: clean(&withheld.notice()),
            cut: Cut::Right,
            detail: Detail::None,
        };
    }
    let input = &tc.input;
    let path = || path_text(str_of(input, "file_path").unwrap_or(""), root);
    match tc.name.as_str() {
        "Read" | "read" => Parts {
            target: read_target(input, root),
            cut: Cut::Left,
            detail: Detail::None,
        },
        "Edit" | "edit" => Parts {
            target: path(),
            cut: Cut::Left,
            detail: edit_detail(tc),
        },
        "Write" | "write" => Parts {
            target: path(),
            cut: Cut::Left,
            detail: write_detail(tc),
        },
        "Bash" | "bash" => Parts {
            target: bash_target(input),
            cut: Cut::Right,
            detail: Detail::None,
        },
        "apply_patch" => {
            let (target, detail) = patch_parts(input, root);
            Parts {
                target,
                cut: Cut::Left,
                detail,
            }
        }
        _ => {
            let (target, cut) = generic_target(input, root);
            Parts {
                target,
                cut,
                detail: Detail::None,
            }
        }
    }
}

/// Elapsed time string (`0.3s`); empty when the call never started.
pub(super) fn elapsed_str(tc: &ToolCallInfo, clock: &dyn Clock) -> String {
    let start = tc.started_at_ms.unwrap_or(0);
    if start == 0 {
        return String::new();
    }
    let end = tc
        .completed_at_ms
        .unwrap_or_else(|| clock.wall_now_ms().max(0) as u64);
    format!("{:.1}s", (end.saturating_sub(start)) as f64 / 1000.0)
}

/// Map a status chip string to its theme color.
fn chip_color(chip: &str, theme: &Theme) -> Color {
    match chip {
        "⋯ Validating" => theme.colors.tool_status_validating,
        "⧖ Scheduled" => theme.colors.tool_status_scheduled,
        "? Awaiting approval" => theme.colors.tool_status_awaiting,
        "● Executing" => theme.colors.tool_status_executing,
        "✓ Success" => theme.colors.tool_status_success,
        "✗ Error" => theme.colors.tool_status_error,
        "⊘ Cancelled" => theme.colors.tool_status_cancelled,
        _ => theme.colors.fg_muted,
    }
}

/// The status glyph: the result decides once the call finished, the chip before
/// that; no glyph while the call is pending.
fn status_glyph(tc: &ToolCallInfo, theme: &Theme) -> Option<(&'static str, Style)> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    match (&tc.result, tc.status.as_deref()) {
        (Some(result), _) if result.is_error => Some(("✗", bold.fg(theme.colors.error))),
        (Some(_), Some(chip @ "⊘ Cancelled")) => {
            Some(("⊘", Style::default().fg(chip_color(chip, theme))))
        }
        (Some(_), _) => Some(("✓", bold.fg(theme.colors.success))),
        (None, Some(chip)) => {
            let glyph = match chip {
                "⋯ Validating" => "⋯",
                "⧖ Scheduled" => "⧖",
                "? Awaiting approval" => "?",
                "● Executing" => "●",
                "✓ Success" => "✓",
                "✗ Error" => "✗",
                "⊘ Cancelled" => "⊘",
                _ => return None,
            };
            Some((glyph, Style::default().fg(chip_color(chip, theme))))
        }
        (None, None) => None,
    }
}

/// A path target that lost its head keeps whole segments when it can.
fn ellipsize_left(text: &str, budget: usize) -> Cow<'_, str> {
    if geom::cells(text) <= budget {
        return Cow::Borrowed(text);
    }
    match budget {
        0 => return Cow::Borrowed(""),
        1 => return Cow::Borrowed(ELLIPSIS),
        _ => {}
    }
    let tail = geom::clip_tail(text, budget - 1);
    let cut_at_boundary = text[..text.len() - tail.len()].ends_with('/');
    let tail = match tail.find('/') {
        Some(i) if i > 0 && !cut_at_boundary => &tail[i..],
        _ => tail,
    };
    Cow::Owned(format!("{ELLIPSIS}{tail}"))
}

fn fit_target(text: &str, cut: Cut, budget: usize) -> Cow<'_, str> {
    match cut {
        Cut::Left => ellipsize_left(text, budget),
        Cut::Right => geom::ellipsize(text, budget, ELLIPSIS),
    }
}

/// The pieces that survive the D14 fit, in drop order: elapsed, target length,
/// detail, verb length.
struct Fit<'a> {
    verb: Cow<'a, str>,
    target: Cow<'a, str>,
    detail: bool,
    elapsed: bool,
}

fn fit<'a>(
    avail: usize,
    glyph: usize,
    verb: &'a str,
    parts: &'a Parts,
    detail: &str,
    elapsed: &str,
) -> Fit<'a> {
    let target = parts.target.as_str();
    let v = geom::cells(verb);
    let t = geom::cells(target);
    let sep = usize::from(t > 0);
    let d = if detail.is_empty() {
        0
    } else {
        1 + geom::cells(detail)
    };
    let e = if elapsed.is_empty() {
        0
    } else {
        3 + geom::cells(elapsed)
    };
    let base = glyph + v;
    let keep = |target: Cow<'a, str>, detail: bool, elapsed: bool| Fit {
        verb: Cow::Borrowed(verb),
        target,
        detail,
        elapsed,
    };
    if base + sep + t + d + e <= avail {
        return keep(Cow::Borrowed(target), d > 0, e > 0);
    }
    if base + sep + t + d <= avail {
        return keep(Cow::Borrowed(target), d > 0, false);
    }
    let floor = t.min(MIN_TARGET_CELLS);
    if d > 0 && base + sep + floor + d <= avail {
        let room = avail - (base + sep + d);
        return keep(fit_target(target, parts.cut, room), true, false);
    }
    if base + sep + floor <= avail {
        let room = avail - (base + sep);
        return keep(fit_target(target, parts.cut, room), false, false);
    }
    // The verb gives way last, down to what the target floor leaves.
    let room = avail.saturating_sub(glyph + sep + floor).max(1);
    Fit {
        verb: geom::ellipsize(verb, room, ELLIPSIS),
        target: fit_target(target, parts.cut, floor),
        detail: false,
        elapsed: false,
    }
}

/// Build the header row of one tool call for `width` cells.
pub(super) fn header_line<'a>(
    tc: &ToolCallInfo,
    theme: &Theme,
    width: usize,
    clock: &dyn Clock,
    withheld: Option<&ToolInputWithheld>,
    root: Option<&Path>,
) -> Line<'a> {
    let is_error = tc.result.as_ref().is_some_and(|result| result.is_error);
    let lead = if is_error {
        Span::styled(
            "┃ ",
            Style::default()
                .fg(theme.colors.error)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "┄ ",
            Style::default().fg(theme.colors.tool_border_collapsed),
        )
    };
    let mut glyph = status_glyph(tc, theme);
    let parts = parts(tc, withheld, root);
    let verb = clean(&display_tool_name(&tc.name));
    let detail_text = parts.detail.text();
    let elapsed = elapsed_str(tc, clock);
    let avail = width.saturating_sub(LEAD_CELLS);
    // A glyph that would leave the verb no cell at all is dropped.
    if avail < 3 {
        glyph = None;
    }
    let glyph_cells = if glyph.is_some() { 2 } else { 0 };
    let mut fitted = fit(avail, glyph_cells, &verb, &parts, &detail_text, &elapsed);
    let used = glyph_cells
        + geom::cells(&fitted.verb)
        + if fitted.target.is_empty() {
            0
        } else {
            1 + geom::cells(&fitted.target)
        }
        + if fitted.detail {
            1 + geom::cells(&detail_text)
        } else {
            0
        }
        + if fitted.elapsed {
            3 + geom::cells(&elapsed)
        } else {
            0
        };
    if used > avail {
        // Narrower than any piece the fit can drop: the verb alone, clipped.
        fitted.target = Cow::Borrowed("");
        fitted.detail = false;
        fitted.elapsed = false;
        fitted.verb = Cow::Owned(geom::clip(&verb, avail.saturating_sub(glyph_cells)).to_owned());
    }

    let mut spans = vec![lead];
    if let Some((mark, style)) = glyph {
        spans.push(Span::styled(format!("{mark} "), style));
    }
    spans.push(Span::styled(
        fitted.verb.into_owned(),
        Style::default()
            .fg(theme.colors.tool_name)
            .add_modifier(Modifier::BOLD),
    ));
    if !fitted.target.is_empty() {
        spans.push(Span::styled(
            format!(" {}", fitted.target),
            Style::default().fg(theme.colors.fg_secondary),
        ));
    }
    if fitted.detail {
        spans.push(Span::raw(" "));
        spans.extend(parts.detail.spans(detail_text, theme));
    }
    if fitted.elapsed {
        spans.push(Span::styled(
            format!(" · {elapsed}"),
            Style::default().fg(theme.colors.fg_muted),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_9_ellipsize_left_keeps_whole_segments_when_it_can() {
        let path = "src/adapters/tui/widgets/chat_pane/reasoning.rs";
        assert_eq!(ellipsize_left(path, 47), path);
        assert_eq!(
            ellipsize_left(path, 45),
            "…/adapters/tui/widgets/chat_pane/reasoning.rs"
        );
        assert_eq!(ellipsize_left(path, 24), "…/chat_pane/reasoning.rs");
        // A single long file name has no segment to keep: plain tail.
        assert_eq!(ellipsize_left("reasoning.rs", 6), "…ng.rs");
        assert_eq!(ellipsize_left(path, 1), "…");
        assert_eq!(ellipsize_left(path, 0), "");
    }

    #[test]
    fn story21_9_clean_removes_controls_and_collapses_every_whitespace_run() {
        assert_eq!(clean("a\u{1b}[31m b\n\t c\u{202e}d"), "a b cd");
        assert_eq!(clean("  \n "), "");
    }

    #[test]
    fn story21_9_generic_target_never_shows_json() {
        let root = None;
        let (target, _) = generic_target(&serde_json::json!({"o": {"a": "nested"}, "n": 1}), root);
        assert_eq!(target, "");
        let (target, cut) =
            generic_target(&serde_json::json!({"path": "/x/y", "query": "q"}), root);
        assert_eq!((target.as_str(), cut), ("/x/y", Cut::Left));
        // An empty preferred value is skipped, not shown as a blank target.
        let (target, _) = generic_target(&serde_json::json!({"query": " ", "zed": "z"}), root);
        assert_eq!(target, "z");
    }

    #[test]
    fn story21_9_patch_parts_counts_the_other_files() {
        let patch = "*** Begin Patch\n*** Add File: a\n*** Delete File: b\n*** Update File: c\n";
        let (first, detail) = patch_parts(&serde_json::json!({ "patch": patch }), None);
        assert_eq!(first, "a");
        assert_eq!(detail, Detail::Text("+2 files".into()));
    }
}
