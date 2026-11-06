//! Original tab sources and their sanitized, literal display boundary.
//!
//! The diff derivation and honest provenance messages formerly belonged to the
//! expanded tool widget. The inspector is their owner after the Stage cutover.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use ratatui::prelude::*;
use serde::Serialize;

use crate::adapters::tui::markdown::{self, RenderOptions};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::{
    DIFF_MAX_LINES, DiffKind, DiffLine, InvocationStatus, NotCapturedReason, WriteDiffState,
    diff::cap_lines, display_diff,
};
use crate::domain::services::peer_text::sanitize_tool_text_block;

use super::model::{ToolTab, ToolView};

/// Output is raw by default, but still uses the shared markdown pipeline.
pub const OUTPUT_OPTIONS: RenderOptions = RenderOptions::literal();

const BASE_TABS: &[ToolTab] = &[ToolTab::Output, ToolTab::Input, ToolTab::Raw];
const DIFF_TABS: &[ToolTab] = &[ToolTab::Output, ToolTab::Input, ToolTab::Raw, ToolTab::Diff];
const NO_CHANGE: &str = "no line changes — the file content is unchanged";

/// Debug-only payload-work counters, scoped to the current render thread.
#[cfg(any(test, debug_assertions))]
pub mod metrics {
    use std::cell::Cell;

    thread_local! {
        static BYTES: Cell<usize> = const { Cell::new(0) };
        static PREPARATIONS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn payload(bytes: usize) {
        BYTES.with(|count| count.set(count.get() + bytes));
    }

    pub(super) fn prepare() {
        PREPARATIONS.with(|count| count.set(count.get() + 1));
    }

    pub fn counts() -> (usize, usize) {
        (BYTES.with(Cell::get), PREPARATIONS.with(Cell::get))
    }

    pub fn reset() {
        BYTES.with(|count| count.set(0));
        PREPARATIONS.with(|count| count.set(0));
    }
}

// Borrow the original record; identity precedes potentially large result text.
#[derive(Serialize)]
struct RawRecord<'a> {
    id: &'a str,
    tool: &'a str,
    status: &'a InvocationStatus,
    input: &'a serde_json::Value,
    result: Option<RawResult<'a>>,
    started_at: i64,
    ended_at: Option<i64>,
}

#[derive(Serialize)]
struct RawResult<'a> {
    content: &'a str,
    is_error: bool,
}

/// Site-supplied rows stay borrowed; memoized input-derived rows share storage.
#[derive(Debug, Clone)]
pub enum DiffLines<'a> {
    Borrowed(&'a [DiffLine]),
    Memoized(Rc<[DiffLine]>),
}

impl AsRef<[DiffLine]> for DiffLines<'_> {
    fn as_ref(&self) -> &[DiffLine] {
        match self {
            Self::Borrowed(lines) => lines,
            Self::Memoized(lines) => lines,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DiffBody<'a> {
    /// Complete, already capped rows, including any elision/cap markers.
    Lines {
        lines: DiffLines<'a>,
    },
    NotCaptured {
        bytes: usize,
        reason: NotCapturedReason,
    },
    NoChange,
}

impl DiffBody<'_> {
    /// Also used by the Stage's content-aware turn-fold heuristic.
    pub fn row_count(&self) -> usize {
        match self {
            Self::Lines { lines } => lines.as_ref().len(),
            Self::NotCaptured { .. } | Self::NoChange => 1,
        }
    }
}

/// Derive a diff for a completed non-error Edit/Write, with no I/O.
///
/// Completion/error/status gating belongs to the caller. The field-oriented
/// API also serves the Stage's fold heuristic without manufacturing a view or
/// copying `ToolCallInfo`/`ToolOutput` between their distinct domain shapes.
pub fn diff_body<'a>(
    name: &str,
    args: &serde_json::Value,
    diff: &'a WriteDiffState,
) -> Option<DiffBody<'a>> {
    match name {
        "Edit" | "edit" => {
            let old = args.get("old_string")?.as_str()?;
            let new = args.get("new_string")?.as_str()?;
            if old.is_empty() || old == new {
                return None;
            }
            Some(computed_body(edit_hunk(old, new)))
        }
        "Write" | "write" => match diff {
            WriteDiffState::NotAWrite => None,
            WriteDiffState::NewFile => {
                let content = args.get("content")?.as_str()?;
                Some(computed_body(edit_hunk("", content)))
            }
            WriteDiffState::Diff { lines, more } if lines.is_empty() && *more == 0 => {
                Some(DiffBody::NoChange)
            }
            WriteDiffState::Diff { lines, .. } => Some(DiffBody::Lines {
                lines: DiffLines::Borrowed(lines),
            }),
            WriteDiffState::NotCaptured { reason } => Some(DiffBody::NotCaptured {
                bytes: args
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, str::len),
                reason: *reason,
            }),
        },
        _ => None,
    }
}

fn computed_body(lines: Rc<[DiffLine]>) -> DiffBody<'static> {
    if lines.is_empty() {
        DiffBody::NoChange
    } else {
        DiffBody::Lines {
            lines: DiffLines::Memoized(lines),
        }
    }
}

/// Bounded memoization of the existing capped display-diff computation.
/// Shared rows avoid copying the entire hunk on each Stage height calculation.
fn edit_hunk(old: &str, new: &str) -> Rc<[DiffLine]> {
    #[cfg(any(test, debug_assertions))]
    metrics::payload(old.len() + new.len());
    const MEMO_CAP: usize = 256;
    thread_local! {
        static MEMO: RefCell<HashMap<u64, Rc<[DiffLine]>>> = RefCell::new(HashMap::new());
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    old.hash(&mut hasher);
    new.hash(&mut hasher);
    let key = hasher.finish();
    MEMO.with(|memo| {
        if let Some(hit) = memo.borrow().get(&key) {
            return Rc::clone(hit);
        }
        let (lines, _) = cap_lines(display_diff(old, new), DIFF_MAX_LINES);
        let lines: Rc<[DiffLine]> = lines.into();
        let mut memo = memo.borrow_mut();
        if memo.len() >= MEMO_CAP {
            memo.clear();
        }
        memo.insert(key, Rc::clone(&lines));
        lines
    })
}

/// No dead Diff tab: a successful invocation AND a non-error result are needed.
pub fn available_tabs(view: &ToolView<'_>) -> &'static [ToolTab] {
    let Some(result) = view.result else {
        return BASE_TABS;
    };
    if *view.status == InvocationStatus::Success
        && !result.is_error
        && diff_body(view.name, view.args, &result.diff).is_some()
    {
        DIFF_TABS
    } else {
        BASE_TABS
    }
}

fn view_diff<'a>(view: &ToolView<'a>) -> Option<DiffBody<'a>> {
    if *view.status != InvocationStatus::Success {
        return None;
    }
    let result = view.result.filter(|result| !result.is_error)?;
    diff_body(view.name, view.args, &result.diff)
}

/// Original bytes only: display notices and sanitization never change a source.
/// In particular an absent Output remains empty rather than becoming copyable
/// UI prose, and a completed result always takes precedence over a live tail.
pub fn text<'a>(view: &ToolView<'a>, tab: ToolTab) -> Cow<'a, str> {
    match tab {
        ToolTab::Output => Cow::Borrowed(
            view.result
                .map(|result| result.content.as_str())
                .or_else(|| view.is_running().then_some(view.tail).flatten())
                .unwrap_or(""),
        ),
        ToolTab::Input => Cow::Owned(
            serde_json::to_string_pretty(view.args).expect("JSON values always serialize"),
        ),
        ToolTab::Raw => {
            let record = RawRecord {
                id: &view.target.tool_call_id,
                tool: view.name,
                status: view.status,
                input: view.args,
                result: view.result.map(|result| RawResult {
                    content: &result.content,
                    is_error: result.is_error,
                }),
                started_at: view.started_at,
                ended_at: view.ended_at,
            };
            Cow::Owned(serde_json::to_string_pretty(&record).expect("JSON values always serialize"))
        }
        ToolTab::Diff => match view_diff(view) {
            Some(DiffBody::Lines { lines }) => Cow::Owned(diff_text(lines.as_ref())),
            Some(DiffBody::NotCaptured { bytes, reason }) => {
                Cow::Owned(not_captured(bytes, reason))
            }
            Some(DiffBody::NoChange) => Cow::Borrowed(NO_CHANGE),
            None => Cow::Borrowed(""),
        },
    }
}

fn not_captured(bytes: usize, reason: NotCapturedReason) -> String {
    let verb = if reason == NotCapturedReason::NoActiveCheckpoint {
        "overwrote"
    } else {
        "wrote"
    };
    format!(
        "{verb} {bytes} bytes — previous content not captured ({})",
        reason.describe()
    )
}

fn diff_prefix(kind: DiffKind) -> &'static str {
    match kind {
        DiffKind::Added => "+ ",
        DiffKind::Removed => "- ",
        DiffKind::Context => "  ",
        DiffKind::Elided => "",
    }
}

fn diff_text(lines: &[DiffLine]) -> String {
    let mut text = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index != 0 {
            text.push('\n');
        }
        text.push_str(diff_prefix(line.kind));
        text.push_str(&line.content);
    }
    text
}

/// Display-only notices are deliberately separate from original tab sources.
/// The injected wall time is used only for the no-tail running notice.
pub fn display_text<'a>(view: &ToolView<'a>, tab: ToolTab, now_ms: i64) -> Cow<'a, str> {
    if tab == ToolTab::Input
        && (view.args.is_null() || view.args.as_object().is_some_and(|args| args.is_empty()))
    {
        return Cow::Borrowed("No input.");
    }
    if tab == ToolTab::Output {
        if let Some(result) = view.result {
            if result.content.is_empty() {
                return Cow::Borrowed("The tool returned no output.");
            }
        } else if view.is_running() {
            let tail = view.tail.unwrap_or("");
            if tail.is_empty() {
                let elapsed = now_ms.saturating_sub(view.started_at).max(0) as f64 / 1000.0;
                return Cow::Owned(format!("No output yet — running ({elapsed:.1}s)"));
            }
            return Cow::Owned(format!(
                "Live tail (last {} lines) — the full output appears when the tool finishes\n{tail}",
                tail.lines().count(),
            ));
        } else {
            return Cow::Borrowed("The tool returned no output.");
        }
    }
    text(view, tab)
}

/// A sanitized cache candidate. The host first compares [`revision`], then
/// prepares changed sources once to derive its sanitized key and render on a
/// cache miss. In particular Input/Raw are not serialized a second time.
pub struct PreparedSource<'a> {
    body: PreparedBody<'a>,
    options: RenderOptions,
    dim_header: bool,
    hash: u64,
}

enum PreparedBody<'a> {
    Literal(Cow<'a, str>),
    Diff(DiffLines<'a>),
    Muted(Cow<'a, str>),
    Empty,
}

impl PreparedSource<'_> {
    /// Hash of sanitized source bytes and their rendering style. Width and
    /// wrap are the frame's remaining key axes, not part of source identity.
    pub fn hash(&self) -> u64 {
        self.hash
    }

    /// Render this already-prepared candidate without reading the view again.
    pub fn render(&self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        match &self.body {
            PreparedBody::Literal(text) => {
                let mut lines =
                    markdown::render(text, usize::from(width), theme, &self.options).lines;
                if self.dim_header {
                    if let Some(header) = lines.first_mut() {
                        for span in &mut header.spans {
                            span.style = span.style.fg(theme.colors.fg_muted);
                        }
                    }
                }
                lines
            }
            PreparedBody::Diff(lines) => render_diff_lines(lines.as_ref(), theme),
            PreparedBody::Muted(text) => vec![Line::from(Span::styled(
                text.to_string(),
                Style::default().fg(theme.colors.fg_muted),
            ))],
            PreparedBody::Empty => Vec::new(),
        }
    }
}

/// Prepare one changed source at the display boundary. Clean borrowed Output
/// remains borrowed; clean owned JSON retains its existing allocation.
pub fn prepare<'a>(view: &ToolView<'a>, tab: ToolTab, now_ms: i64) -> PreparedSource<'a> {
    #[cfg(any(test, debug_assertions))]
    metrics::prepare();
    let body = if tab == ToolTab::Diff {
        match view_diff(view) {
            Some(DiffBody::Lines { lines }) => PreparedBody::Diff(lines),
            Some(DiffBody::NotCaptured { bytes, reason }) => {
                PreparedBody::Muted(Cow::Owned(not_captured(bytes, reason)))
            }
            Some(DiffBody::NoChange) => PreparedBody::Muted(Cow::Borrowed(NO_CHANGE)),
            None => PreparedBody::Empty,
        }
    } else {
        PreparedBody::Literal(sanitize_source(display_text(view, tab, now_ms)))
    };
    let options = if tab == ToolTab::Output {
        OUTPUT_OPTIONS
    } else {
        RenderOptions::literal()
    };
    let dim_header = tab == ToolTab::Output && view.result.is_none() && view.is_running();
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    dim_header.hash(&mut hash);
    match &body {
        PreparedBody::Literal(text) => {
            #[cfg(any(test, debug_assertions))]
            metrics::payload(text.len());
            0_u8.hash(&mut hash);
            text.hash(&mut hash);
        }
        PreparedBody::Diff(lines) => {
            1_u8.hash(&mut hash);
            for (index, line) in lines.as_ref().iter().enumerate() {
                if index != 0 {
                    hash.write(b"\n");
                }
                // Prefixes and elision rows retain their distinct color/style
                // semantics even if unusual content produces identical text.
                std::mem::discriminant(&line.kind).hash(&mut hash);
                hash.write(diff_prefix(line.kind).as_bytes());
                hash.write(sanitize_tool_text_block(&line.content).as_bytes());
            }
        }
        PreparedBody::Muted(text) => {
            2_u8.hash(&mut hash);
            text.hash(&mut hash);
        }
        PreparedBody::Empty => 3_u8.hash(&mut hash),
    }
    PreparedSource {
        body,
        options,
        dim_header,
        hash: hash.finish(),
    }
}

fn sanitize_source(source: Cow<'_, str>) -> Cow<'_, str> {
    match source {
        Cow::Borrowed(source) => sanitize_tool_text_block(source),
        Cow::Owned(source) => match sanitize_tool_text_block(&source) {
            Cow::Borrowed(_) => Cow::Owned(source),
            Cow::Owned(sanitized) => Cow::Owned(sanitized),
        },
    }
}

/// Convenience entry point over the same preparation/rendering implementation
/// used by frame-owned caches.
pub fn render(
    view: &ToolView<'_>,
    tab: ToolTab,
    width: u16,
    theme: &Theme,
    now_ms: i64,
) -> Vec<Line<'static>> {
    prepare(view, tab, now_ms).render(width, theme)
}

/// The moved diff renderer: no output gutter, and elision rows carry no prefix.
/// Sanitize before tab expansion; the original source bytes remain untouched.
pub fn render_diff_lines(diff: &[DiffLine], theme: &Theme) -> Vec<Line<'static>> {
    diff.iter()
        .map(|line| {
            let color = match line.kind {
                DiffKind::Added => theme.colors.success,
                DiffKind::Removed => theme.colors.error,
                DiffKind::Context => theme.colors.fg_secondary,
                DiffKind::Elided => theme.colors.fg_muted,
            };
            let mut source = format!("{}{}", diff_prefix(line.kind), line.content);
            if let Cow::Owned(safe) = sanitize_tool_text_block(&source) {
                source = safe;
            }
            if let Cow::Owned(expanded) = geom::expand_tabs(&source, 0) {
                source = expanded;
            }
            Line::from(Span::styled(source, Style::default().fg(color)))
        })
        .collect()
}

/// Constant-size canonical identity; no source bytes are inspected on a hit.
/// The host adds width and, for a no-tail running notice, its elapsed-time tick.
pub fn revision(view: &ToolView<'_>, tab: ToolTab) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    view.source_revision.hash(&mut hash);
    tab.hash(&mut hash);
    if tab == ToolTab::Output && view.is_running() && view.result.is_none() {
        view.tail_revision.hash(&mut hash);
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::super::model::ToolTarget;
    use super::*;
    use crate::domain::models::{PartId, ToolOutput, TurnId};
    use serde_json::json;

    fn with_view(
        name: &str,
        args: serde_json::Value,
        status: InvocationStatus,
        result: Option<ToolOutput>,
        check: impl FnOnce(ToolView<'_>),
    ) {
        let target = ToolTarget {
            conversation_id: "conversation".into(),
            tool_call_id: "tc_turn_2".into(),
        };
        let turn_id = TurnId("turn".into());
        check(ToolView {
            target: &target,
            turn_id: &turn_id,
            part_id: PartId(2),
            name,
            status: &status,
            ordinal: 2,
            args: &args,
            result: result.as_ref(),
            tail: None,
            started_at: 1_000,
            ended_at: Some(2_000),
            source_revision: 1,
            tail_revision: 0,
        });
    }

    fn output(content: &str, diff: WriteDiffState) -> ToolOutput {
        ToolOutput {
            content: content.into(),
            is_error: false,
            diff,
            images: Vec::new(),
        }
    }

    fn rendered(view: &ToolView<'_>, tab: ToolTab) -> Vec<String> {
        render(view, tab, 200, &Theme::dark(), 2_500)
            .iter()
            .map(Line::to_string)
            .collect()
    }

    #[test]
    fn story21_5_edit_hunk_preserves_order_colors_and_output_source() {
        with_view(
            "Edit",
            json!({"old_string":"a\nb", "new_string":"a\nc"}),
            InvocationStatus::Success,
            Some(output("Successfully edited", WriteDiffState::NotAWrite)),
            |view| {
                assert_eq!(available_tabs(&view), DIFF_TABS);
                assert_eq!(rendered(&view, ToolTab::Diff), ["  a", "- b", "+ c"]);
                assert_eq!(text(&view, ToolTab::Diff), "  a\n- b\n+ c");
                assert_eq!(text(&view, ToolTab::Output), "Successfully edited");
                let theme = Theme::dark();
                let lines = render(&view, ToolTab::Diff, 80, &theme, 2_500);
                for (line, color) in lines.iter().zip([
                    theme.colors.fg_secondary,
                    theme.colors.error,
                    theme.colors.success,
                ]) {
                    assert_eq!(line.spans[0].style.fg, Some(color));
                }
            },
        );
    }

    #[test]
    fn story21_5_write_new_overwrite_and_cap_preserve_diff_rows() {
        for (args, diff, expected) in [
            (
                json!({"content":"fn a() {}\nfn b() {}"}),
                WriteDiffState::NewFile,
                vec!["+ fn a() {}", "+ fn b() {}"],
            ),
            (
                json!({"content":"x\nz"}),
                WriteDiffState::from_original(b"x\ny", "x\nz"),
                vec!["  x", "- y", "+ z"],
            ),
        ] {
            with_view(
                "Write",
                args,
                InvocationStatus::Success,
                Some(output("wrote", diff)),
                |view| {
                    assert_eq!(available_tabs(&view), DIFF_TABS);
                    assert_eq!(rendered(&view, ToolTab::Diff), expected);
                },
            );
        }
        let content: String = (0..500).map(|i| format!("line{i}\n")).collect();
        with_view(
            "Write",
            json!({"content":content}),
            InvocationStatus::Success,
            Some(output("wrote", WriteDiffState::NewFile)),
            |view| {
                let rows = rendered(&view, ToolTab::Diff);
                assert_eq!(
                    rows.iter().filter(|row| row.starts_with("+ ")).count(),
                    DIFF_MAX_LINES
                );
                assert!(rows.iter().any(|row| row == "… 300 more lines"));
                assert_eq!(rows.len(), DIFF_MAX_LINES + 1);
            },
        );
    }

    #[test]
    fn story21_5_write_no_change_and_missing_capture_are_honest() {
        let reasons = [
            NotCapturedReason::NoActiveCheckpoint,
            NotCapturedReason::SnapshotUnavailable,
            NotCapturedReason::SnapshotReadFailed,
            NotCapturedReason::OriginalNotUtf8,
            NotCapturedReason::SupersededInBatch,
            NotCapturedReason::HistoricalOrReattached,
        ];
        for reason in reasons {
            with_view(
                "Write",
                json!({"content":"界"}),
                InvocationStatus::Success,
                Some(output("wrote", WriteDiffState::NotCaptured { reason })),
                |view| {
                    let verb = if reason == NotCapturedReason::NoActiveCheckpoint {
                        "overwrote"
                    } else {
                        "wrote"
                    };
                    let expected = format!(
                        "{verb} 3 bytes — previous content not captured ({})",
                        reason.describe()
                    );
                    assert_eq!(text(&view, ToolTab::Diff), expected);
                    assert_eq!(rendered(&view, ToolTab::Diff), [expected]);
                    assert_eq!(available_tabs(&view), DIFF_TABS);
                    let theme = Theme::dark();
                    let rows = render(&view, ToolTab::Diff, 80, &theme, 0);
                    assert!(
                        rows[0]
                            .spans
                            .iter()
                            .all(|span| span.style.fg == Some(theme.colors.fg_muted))
                    );
                },
            );
        }
        for diff in [
            WriteDiffState::Diff {
                lines: Vec::new(),
                more: 0,
            },
            WriteDiffState::NewFile,
        ] {
            with_view(
                "Write",
                json!({"content":""}),
                InvocationStatus::Success,
                Some(output("", diff)),
                |view| {
                    assert_eq!(text(&view, ToolTab::Diff), NO_CHANGE);
                    assert_eq!(rendered(&view, ToolTab::Diff), [NO_CHANGE]);
                },
            );
        }
    }

    #[test]
    fn story21_5_diff_tab_requires_success_valid_input_and_body() {
        for name in ["Edit", "Write", "Bash"] {
            for status in [
                InvocationStatus::Pending,
                InvocationStatus::Running,
                InvocationStatus::Error,
                InvocationStatus::Cancelled,
            ] {
                with_view(
                    name,
                    json!({"old_string":"old", "new_string":"new", "content":"new"}),
                    status,
                    Some(output("result", WriteDiffState::NewFile)),
                    |view| {
                        assert_eq!(available_tabs(&view), BASE_TABS);
                        assert!(text(&view, ToolTab::Diff).is_empty());
                    },
                );
            }
        }
        for (name, args, diff) in [
            ("Bash", json!({}), WriteDiffState::NotAWrite),
            (
                "Edit",
                json!({"old_string":"", "new_string":"new"}),
                WriteDiffState::NotAWrite,
            ),
            (
                "Edit",
                json!({"old_string":"same", "new_string":"same"}),
                WriteDiffState::NotAWrite,
            ),
            (
                "Edit",
                json!({"old_string":"old"}),
                WriteDiffState::NotAWrite,
            ),
            ("Write", json!({"content":"new"}), WriteDiffState::NotAWrite),
            ("Write", json!({}), WriteDiffState::NewFile),
        ] {
            with_view(
                name,
                args,
                InvocationStatus::Success,
                Some(output("result", diff)),
                |view| {
                    assert_eq!(available_tabs(&view), BASE_TABS);
                },
            );
        }
        let mut failed = output("failed", WriteDiffState::NewFile);
        failed.is_error = true;
        with_view(
            "Write",
            json!({"content":"new"}),
            InvocationStatus::Success,
            Some(failed),
            |view| {
                assert_eq!(available_tabs(&view), BASE_TABS);
            },
        );
        with_view(
            "Edit",
            json!({"old_string":"old", "new_string":"new"}),
            InvocationStatus::Success,
            None,
            |view| {
                assert_eq!(available_tabs(&view), BASE_TABS);
            },
        );
    }

    #[test]
    fn story21_5_literal_display_is_sanitized_original_sources_are_not() {
        let original = "# literal * |\r\n\x1b[31mred\x1b[0m\tx\u{202e}evil\x07\n界e\u{301}👩‍💻";
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output(original, WriteDiffState::NotAWrite)),
            |view| {
                assert!(matches!(text(&view, ToolTab::Output), Cow::Borrowed(_)));
                assert_eq!(text(&view, ToolTab::Output), original);
                let rows = rendered(&view, ToolTab::Output).join("\n");
                for control in ['\x1b', '\x07', '\u{202e}', '\t', '\r'] {
                    assert!(!rows.contains(control), "{rows:?}");
                }
                assert!(rows.contains("# literal * |\nred"));
                assert!(rows.contains("界e\u{301}👩‍💻"));
                let safe = sanitize_tool_text_block(original);
                assert_eq!(
                    render(&view, ToolTab::Output, 200, &Theme::dark(), 0),
                    markdown::render(&safe, 200, &Theme::dark(), &RenderOptions::literal()).lines
                );
            },
        );
    }

    #[test]
    fn story21_5_empty_notices_never_replace_original_bytes() {
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Running,
            None,
            |mut view| {
                assert_eq!(text(&view, ToolTab::Output), "");
                assert_eq!(
                    display_text(&view, ToolTab::Output, 2_500),
                    "No output yet — running (1.5s)"
                );
                assert_eq!(display_text(&view, ToolTab::Input, 0), "No input.");
                assert_eq!(text(&view, ToolTab::Input), "{}");
                view.tail = Some("A1\nA2");
                assert_eq!(text(&view, ToolTab::Output), "A1\nA2");
                assert_eq!(
                    display_text(&view, ToolTab::Output, 0),
                    "Live tail (last 2 lines) — the full output appears when the tool finishes\nA1\nA2"
                );
            },
        );
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output("", WriteDiffState::NotAWrite)),
            |view| {
                assert_eq!(text(&view, ToolTab::Output), "");
                assert_eq!(
                    display_text(&view, ToolTab::Output, 0),
                    "The tool returned no output."
                );
            },
        );
    }

    #[test]
    fn story21_5_raw_is_exact_projected_record_and_revision_tracks_sources() {
        with_view(
            "Bash",
            json!({"command":"printf hi"}),
            InvocationStatus::Success,
            Some(output("hi\u{1b}[31m", WriteDiffState::NotAWrite)),
            |mut view| {
                let raw: serde_json::Value =
                    serde_json::from_str(&text(&view, ToolTab::Raw)).unwrap();
                assert_eq!(
                    raw,
                    json!({
                        "id":"tc_turn_2", "tool":"Bash", "status":"success",
                        "input":{"command":"printf hi"},
                        "result":{"content":"hi\u{1b}[31m", "is_error":false},
                        "started_at":1000, "ended_at":2000,
                    })
                );
                let revisions = [ToolTab::Output, ToolTab::Input, ToolTab::Raw]
                    .map(|tab| prepare(&view, tab, 0).hash());
                view.tail = Some("irrelevant after completion");
                assert_eq!(
                    revisions,
                    [ToolTab::Output, ToolTab::Input, ToolTab::Raw]
                        .map(|tab| prepare(&view, tab, 0).hash())
                );
                let changed = output("changed", WriteDiffState::NotAWrite);
                let view = ToolView {
                    result: Some(&changed),
                    ..view
                };
                assert_ne!(revisions[0], prepare(&view, ToolTab::Output, 0).hash());
                assert_eq!(revisions[1], prepare(&view, ToolTab::Input, 0).hash());
                assert_ne!(revisions[2], prepare(&view, ToolTab::Raw, 0).hash());
            },
        );
        with_view(
            "Bash",
            json!(null),
            InvocationStatus::Running,
            None,
            |view| {
                let raw: serde_json::Value =
                    serde_json::from_str(&text(&view, ToolTab::Raw)).unwrap();
                assert!(raw["result"].is_null());
            },
        );
        // Private borrowed serialization must remain value-equivalent to A3's
        // json! projection, including large Unicode results and absent fields.
        let large = "界e\u{301}👩‍💻\r\n".repeat(4096);
        for (status, content) in [
            (InvocationStatus::Pending, None),
            (InvocationStatus::Running, None),
            (InvocationStatus::Success, Some(large.as_str())),
            (
                InvocationStatus::Error,
                Some("error \"quoted\"\n\u{1b}[31m"),
            ),
            (InvocationStatus::Cancelled, Some("")),
        ] {
            let mut result = content.map(|text| output(text, WriteDiffState::NotAWrite));
            if let Some(result) = &mut result {
                result.is_error = status == InvocationStatus::Error;
            }
            with_view(
                "工具👩‍💻",
                json!({"unicode":"界e\u{301}👩‍💻", "nested":[null, true, 42], "crlf":"a\r\nb"}),
                status,
                result,
                |mut view| {
                    if view.is_running() {
                        view.ended_at = None;
                    }
                    let baseline = json!({
                        "id":view.target.tool_call_id, "tool":view.name, "status":view.status,
                        "input":view.args,
                        "result":view.result.map(|r| json!({"content":r.content, "is_error":r.is_error})),
                        "started_at":view.started_at, "ended_at":view.ended_at,
                    });
                    let raw = text(&view, ToolTab::Raw);
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
                        baseline
                    );
                    assert!(raw.find("\"tool\":").unwrap() < raw.find("\"result\":").unwrap());
                },
            );
        }
    }

    #[test]
    fn story21_5_diff_sanitizes_and_expands_tabs_without_mutating_source() {
        let diff = vec![DiffLine {
            kind: DiffKind::Added,
            content: "\x1b[31mx\t界\u{2066}".into(),
        }];
        let rows = render_diff_lines(&diff, &Theme::dark());
        assert_eq!(rows[0].to_string(), "+ x     界");
        assert_eq!(diff_text(&diff), "+ \x1b[31mx\t界\u{2066}");
    }

    #[test]
    fn story21_5_diff_rows_stay_borrowed_and_fold_height_matches_body() {
        let args = json!({"content": "after"});
        let diff = WriteDiffState::from_original(b"before", "after");
        let body = diff_body("write", &args, &diff).unwrap();
        let DiffBody::Lines { lines } = &body else {
            panic!("site-supplied lines");
        };
        let WriteDiffState::Diff {
            lines: original, ..
        } = &diff
        else {
            panic!("site-supplied diff");
        };
        assert!(std::ptr::eq(lines.as_ref(), original.as_slice()));
        assert_eq!(
            body.row_count(),
            render_diff_lines(original, &Theme::dark()).len()
        );
        assert_eq!(DiffBody::NoChange.row_count(), 1);
        assert_eq!(
            DiffBody::NotCaptured {
                bytes: 10,
                reason: NotCapturedReason::SnapshotReadFailed,
            }
            .row_count(),
            1,
        );
    }

    #[test]
    fn story21_5_line_ending_change_and_elision_keep_honest_markers() {
        with_view(
            "edit",
            json!({"old_string": "same\n", "new_string": "same"}),
            InvocationStatus::Success,
            Some(output("edited", WriteDiffState::NotAWrite)),
            |view| {
                assert_eq!(available_tabs(&view), DIFF_TABS);
                assert_eq!(
                    rendered(&view, ToolTab::Diff),
                    ["⚠ line endings or trailing newline changed — no line content changed"],
                );
            },
        );
        let original: String = (0..40).map(|i| format!("line{i}\n")).collect();
        let changed = original.replace("line20\n", "changed\n");
        with_view(
            "Write",
            json!({"content": changed}),
            InvocationStatus::Success,
            Some(output(
                "wrote",
                WriteDiffState::from_original(original.as_bytes(), &changed),
            )),
            |view| {
                let rows = rendered(&view, ToolTab::Diff);
                assert!(rows.iter().any(|row| row.starts_with("⋯ ")));
                assert!(rows.iter().any(|row| row == "- line20"));
                assert!(rows.iter().any(|row| row == "+ changed"));
                assert!(!rows.iter().any(|row| row.starts_with("  ⋯ ")));
            },
        );
    }

    #[test]
    fn story21_5_tail_revision_is_call_source_only_and_input_revision_changes() {
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Running,
            None,
            |mut view| {
                let output_revision = prepare(&view, ToolTab::Output, 0).hash();
                let raw_revision = prepare(&view, ToolTab::Raw, 0).hash();
                let input_revision = prepare(&view, ToolTab::Input, 0).hash();
                view.tail = Some("this call's live output");
                assert_ne!(prepare(&view, ToolTab::Output, 0).hash(), output_revision);
                assert_eq!(prepare(&view, ToolTab::Raw, 0).hash(), raw_revision);
                assert_eq!(prepare(&view, ToolTab::Input, 0).hash(), input_revision);
                let args = json!({"command": "changed"});
                let view = ToolView {
                    args: &args,
                    ..view
                };
                assert_ne!(prepare(&view, ToolTab::Input, 0).hash(), input_revision);
                assert_ne!(prepare(&view, ToolTab::Raw, 0).hash(), raw_revision);
            },
        );
    }

    #[test]
    fn story21_5_prepared_source_borrows_clean_output_and_hashes_sanitized_bytes() {
        with_view(
            "Bash",
            json!({}),
            InvocationStatus::Success,
            Some(output("plain", WriteDiffState::NotAWrite)),
            |view| {
                let prepared = prepare(&view, ToolTab::Output, 0);
                assert!(matches!(
                    &prepared.body,
                    PreparedBody::Literal(Cow::Borrowed("plain"))
                ));
                let changed = output("\x1b[31mplain\x1b[0m", WriteDiffState::NotAWrite);
                let changed_view = ToolView {
                    result: Some(&changed),
                    source_revision: view.source_revision + 1,
                    ..view
                };
                assert_ne!(
                    revision(&view, ToolTab::Output),
                    revision(&changed_view, ToolTab::Output),
                );
                let sanitized = prepare(&changed_view, ToolTab::Output, 0);
                assert_eq!(prepared.hash(), sanitized.hash());
                assert_eq!(
                    prepared.render(80, &Theme::dark()),
                    sanitized.render(80, &Theme::dark()),
                );
            },
        );
    }

    #[test]
    fn story21_5_prepared_json_renders_once_without_repreparing() {
        with_view(
            "Bash",
            json!({"command": "echo hi"}),
            InvocationStatus::Success,
            Some(output("hi", WriteDiffState::NotAWrite)),
            |view| {
                let prepared = prepare(&view, ToolTab::Raw, 0);
                let PreparedBody::Literal(Cow::Owned(json)) = &prepared.body else {
                    panic!("pretty record owns its serialized allocation");
                };
                let record: serde_json::Value = serde_json::from_str(json).unwrap();
                assert_eq!(record["result"]["content"], "hi");
                let renders = markdown::metrics::renders();
                let lines = prepared.render(80, &Theme::dark());
                assert_eq!(markdown::metrics::renders(), renders + 1);
                assert!(!lines.is_empty());
                assert_eq!(lines, render(&view, ToolTab::Raw, 80, &Theme::dark(), 0),);
            },
        );
    }

    #[test]
    fn story21_5_prepared_diff_hash_sanitizes_rows_and_retains_style() {
        with_view(
            "Write",
            json!({"content": "plain"}),
            InvocationStatus::Success,
            Some(output(
                "wrote",
                WriteDiffState::Diff {
                    lines: vec![DiffLine {
                        kind: DiffKind::Added,
                        content: "plain".into(),
                    }],
                    more: 0,
                },
            )),
            |view| {
                let clean = prepare(&view, ToolTab::Diff, 0);
                let changed = output(
                    "wrote",
                    WriteDiffState::Diff {
                        lines: vec![DiffLine {
                            kind: DiffKind::Added,
                            content: "\x1b[31mplain\x1b[0m".into(),
                        }],
                        more: 0,
                    },
                );
                let changed_view = ToolView {
                    result: Some(&changed),
                    ..view
                };
                let sanitized = prepare(&changed_view, ToolTab::Diff, 0);
                assert_eq!(clean.hash(), sanitized.hash());
                assert_eq!(
                    clean.render(80, &Theme::dark()),
                    sanitized.render(80, &Theme::dark()),
                );
                let removed = output(
                    "wrote",
                    WriteDiffState::Diff {
                        lines: vec![DiffLine {
                            kind: DiffKind::Removed,
                            content: "plain".into(),
                        }],
                        more: 0,
                    },
                );
                let removed_view = ToolView {
                    result: Some(&removed),
                    ..view
                };
                assert_ne!(
                    clean.hash(),
                    prepare(&removed_view, ToolTab::Diff, 0).hash()
                );
            },
        );
    }
}
