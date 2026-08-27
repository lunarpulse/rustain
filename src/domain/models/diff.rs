//! Line-diff model for Write/Edit tool-block display (Story 19.1, FR30).
//!
//! Pure domain computation — no ratatui, no I/O. The rendering half
//! (`render_diff_lines`) lives in the TUI widget (`tool_block.rs`); this
//! module holds the data shape because [`WriteDiffState`] persists with the
//! conversation (UI-only — never on the provider-facing `ToolResult`).
//!
//! ## Why the state is an enum and not `Option<Vec<DiffLine>>`
//!
//! Code review of 19.1 (Decision 2, Code Review Crew unanimous 4/4) found the
//! original `Option<Vec<DiffLine>>` channel encoded THREE meanings in two
//! unambiguous slots: `None` conflated "not a Write" / "new file" / "recorded
//! before this feature" / "re-projected on attach", and `Some(vec![])` was used
//! as a "no snapshot" sentinel even though `compute_diff` legitimately returns
//! an empty vector. Both collisions were reachable and both made the widget
//! state something false, which is exactly what ruling A4 forbids. The states
//! are now distinct values, so no consumer has to guess and every "no diff"
//! render can name its real reason.

use serde::{Deserialize, Serialize};

/// Maximum diff lines a tool block shows before the `… N more lines` marker
/// (ruling A6). Applied at the producing site as well as the widget so the
/// journal never carries an unbounded diff.
pub const DIFF_MAX_LINES: usize = 200;

/// Unchanged lines kept on each side of a change when eliding (classic
/// unified-diff context).
pub const DIFF_CONTEXT_LINES: usize = 3;

/// Cell budget for the LCS table. Exceeding it degrades the *changed middle*
/// to all-removed + all-added; the common prefix/suffix are already trimmed by
/// then, so a large file with a small change never reaches this.
const LCS_MAX_CELLS: usize = 4_000_000;

/// Kind of a single diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Removed,
    Context,
    /// A collapsed run of unchanged lines, or a note about a change a line
    /// diff cannot show. `content` is the already-rendered muted text.
    Elided,
}

/// One line of a line-level diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub content: String,
}

impl DiffLine {
    fn of(kind: DiffKind, content: impl Into<String>) -> Self {
        DiffLine {
            kind,
            content: content.into(),
        }
    }

    /// Whether this line represents an actual content change.
    pub fn is_change(&self) -> bool {
        matches!(self.kind, DiffKind::Added | DiffKind::Removed)
    }
}

/// Why a completed Write has no line diff to show.
///
/// Every variant renders its own parenthetical, so the widget can never
/// claim "no active checkpoint" for a failure that had nothing to do with
/// checkpoints (code review finding: that false statement was reachable
/// through a snapshot read error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotCapturedReason {
    /// No checkpoint was active, so `execute_write` took no snapshot. This is
    /// the case ruling A4 and AC4 describe.
    NoActiveCheckpoint,
    /// A checkpoint was active but no snapshot exists for this path.
    SnapshotUnavailable,
    /// The snapshot exists but could not be read or decoded.
    SnapshotReadFailed,
    /// The original is not valid UTF-8, so a line diff would be meaningless.
    OriginalNotUtf8,
    /// An earlier Write to this same path in the same checkpoint already owns
    /// the snapshot, so the original for *this* call cannot be established.
    /// Snapshots are first-write-wins per `(checkpoint, path)`.
    SupersededInBatch,
    /// Recorded before Story 19.1, or re-projected by a client that did not
    /// carry the field. Provenance is unknown, so nothing may be inferred.
    HistoricalOrReattached,
}

impl NotCapturedReason {
    /// Parenthetical shown after `previous content not captured`.
    pub fn describe(self) -> &'static str {
        match self {
            NotCapturedReason::NoActiveCheckpoint => "no active checkpoint",
            NotCapturedReason::SnapshotUnavailable => "snapshot unavailable",
            NotCapturedReason::SnapshotReadFailed => "snapshot read failed",
            NotCapturedReason::OriginalNotUtf8 => "original is not valid UTF-8",
            NotCapturedReason::SupersededInBatch => "superseded by another write in the same batch",
            NotCapturedReason::HistoricalOrReattached => {
                "recorded before this feature, or re-projected on attach"
            }
        }
    }
}

/// Display-diff state riding a tool result (Story 19.1, FR30).
///
/// UI-only: it is journaled and persisted with the conversation but never
/// reaches the provider-facing `ToolResult` (ruling A5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WriteDiffState {
    /// Not a Write. Edit derives its hunk from its own input at the widget
    /// (ruling A2); every other tool renders its output text as before.
    NotAWrite,
    /// The Write created the file (or replaced an empty one), so every line of
    /// the new content is an addition and there is nothing to remove. The
    /// widget derives the lines from the tool input — no plumbing needed.
    NewFile,
    /// A real diff against the captured original, already elided and capped at
    /// [`DIFF_MAX_LINES`]; `more` counts lines dropped by the cap.
    ///
    /// An empty `lines` with `more == 0` is a legitimate value meaning "the
    /// write changed nothing" — it is NOT a sentinel for missing capture.
    Diff { lines: Vec<DiffLine>, more: usize },
    /// The write completed but the original could not be established. The
    /// widget renders one honest line naming `reason` and never guesses.
    NotCaptured { reason: NotCapturedReason },
}

/// A missing field on a persisted or re-projected record means the provenance
/// is unknown — never that the file was new. Fabricating `NewFile` here is
/// what painted an all-additions diff over a replaced file.
impl Default for WriteDiffState {
    fn default() -> Self {
        WriteDiffState::NotCaptured {
            reason: NotCapturedReason::HistoricalOrReattached,
        }
    }
}

impl WriteDiffState {
    /// Build the state for a completed Write whose original content is known.
    ///
    /// Handles the honest edge cases the review found: a non-UTF8 original
    /// cannot be line-diffed, and an empty original is genuinely a new-file
    /// render rather than a diff with nothing on the left.
    pub fn from_original(original: &[u8], new_content: &str) -> Self {
        if original.is_empty() {
            return WriteDiffState::NewFile;
        }
        let Ok(original) = std::str::from_utf8(original) else {
            return WriteDiffState::NotCaptured {
                reason: NotCapturedReason::OriginalNotUtf8,
            };
        };
        let (lines, more) = cap_lines(display_diff(original, new_content), DIFF_MAX_LINES);
        WriteDiffState::Diff { lines, more }
    }
}

/// Compute the diff a tool block should show: a line diff, annotated when the
/// change is invisible to a line diff, with long unchanged runs collapsed.
///
/// Elision is not cosmetic. `compute_diff` emits whole-file order, and the
/// display cap keeps the FIRST [`DIFF_MAX_LINES`]; without elision any change
/// past line 200 was replaced by 200 dim context lines and the user saw no
/// change at all — FR30's promise failing silently on any large file.
pub fn display_diff(original: &str, new_content: &str) -> Vec<DiffLine> {
    let mut lines = compute_diff(original, new_content);
    // A line diff cannot see a changed line terminator: `str::lines()` drops a
    // single trailing newline and strips `\r`. Say so rather than render a
    // wall of unchanged context for a write that really did change bytes.
    if original != new_content && !lines.iter().any(DiffLine::is_change) {
        lines.insert(
            0,
            DiffLine::of(
                DiffKind::Elided,
                "⚠ line endings or trailing newline changed — no line content changed",
            ),
        );
    }
    elide_unchanged(lines, DIFF_CONTEXT_LINES)
}

/// Collapse runs of unchanged lines further than `context` from any change
/// into a single `Elided` marker.
pub fn elide_unchanged(lines: Vec<DiffLine>, context: usize) -> Vec<DiffLine> {
    if !lines.iter().any(DiffLine::is_change) {
        // Nothing changed: there is no hunk to centre context on, so keep the
        // annotation lines only and let the caller render an honest summary.
        return lines
            .into_iter()
            .filter(|l| l.kind == DiffKind::Elided)
            .collect();
    }
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if line.is_change() || line.kind == DiffKind::Elided {
            let lo = i.saturating_sub(context);
            let hi = (i + context).min(lines.len() - 1);
            keep[lo..=hi].iter_mut().for_each(|k| *k = true);
        }
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    for (i, line) in lines.into_iter().enumerate() {
        if keep[i] {
            if dropped > 0 {
                out.push(DiffLine::of(
                    DiffKind::Elided,
                    format!("⋯ {} unchanged lines", dropped),
                ));
                dropped = 0;
            }
            out.push(line);
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        out.push(DiffLine::of(
            DiffKind::Elided,
            format!("⋯ {} unchanged lines", dropped),
        ));
    }
    out
}

/// Truncate to `max` content lines, returning the number dropped.
///
/// Takes a HEAD and a TAIL rather than just the head, with an inline marker
/// between them. A wholesale rewrite emits every removal before every
/// addition, so a head-only cap showed 200 red lines and hid every single
/// addition — the file looked deleted. Elision cannot help there: with no
/// common lines there are no unchanged runs to collapse.
pub fn cap_lines(lines: Vec<DiffLine>, max: usize) -> (Vec<DiffLine>, usize) {
    if lines.len() <= max {
        return (lines, 0);
    }
    let dropped = lines.len() - max;
    let head = max / 2;
    let tail = max - head;
    let tail_start = lines.len() - tail;
    let mut out = Vec::with_capacity(max + 1);
    out.extend(lines[..head].iter().cloned());
    out.push(DiffLine::of(
        DiffKind::Elided,
        format!("… {} more lines", dropped),
    ));
    out.extend(lines[tail_start..].iter().cloned());
    (out, dropped)
}

/// Compute a line-by-line diff using longest common subsequence.
///
/// The common prefix and suffix are trimmed to `Context` first: it makes the
/// common "large file, small change" case linear, keeps the LCS table small
/// enough that the degrade branch is effectively unreachable in practice, and
/// yields the diff a reader expects.
pub fn compute_diff(original: &str, new_content: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    if old_lines.is_empty() {
        // New file — all lines are additions
        return new_lines
            .iter()
            .map(|l| DiffLine::of(DiffKind::Added, *l))
            .collect();
    }

    // Trim the common prefix, then the common suffix, of the untouched region.
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];

    let mut result = Vec::with_capacity(old_lines.len().max(new_lines.len()));
    result.extend(
        old_lines[..prefix]
            .iter()
            .map(|l| DiffLine::of(DiffKind::Context, *l)),
    );
    result.extend(diff_middle(old_mid, new_mid));
    result.extend(
        old_lines[old_lines.len() - suffix..]
            .iter()
            .map(|l| DiffLine::of(DiffKind::Context, *l)),
    );
    result
}

/// Diff the changed middle once the common prefix/suffix are removed.
fn diff_middle(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffLine> {
    let m = old_lines.len();
    let n = new_lines.len();
    if m == 0 {
        return new_lines
            .iter()
            .map(|l| DiffLine::of(DiffKind::Added, *l))
            .collect();
    }
    if n == 0 {
        return old_lines
            .iter()
            .map(|l| DiffLine::of(DiffKind::Removed, *l))
            .collect();
    }

    // Degrade only if the trimmed middle is still enormous. Removals first
    // then additions, so the display cap shows the removals AND the elision
    // marker rather than silently hiding every addition.
    if (m + 1).saturating_mul(n + 1) > LCS_MAX_CELLS {
        let mut result = Vec::with_capacity(m + n);
        result.extend(
            old_lines
                .iter()
                .map(|l| DiffLine::of(DiffKind::Removed, *l)),
        );
        result.extend(new_lines.iter().map(|l| DiffLine::of(DiffKind::Added, *l)));
        return result;
    }

    // LCS table, flat: one allocation instead of m+1 nested Vecs.
    let stride = n + 1;
    let mut dp = vec![0u32; (m + 1) * stride];
    for i in 1..=m {
        for j in 1..=n {
            dp[i * stride + j] = if old_lines[i - 1] == new_lines[j - 1] {
                dp[(i - 1) * stride + (j - 1)] + 1
            } else {
                dp[(i - 1) * stride + j].max(dp[i * stride + (j - 1)])
            };
        }
    }

    let mut result = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_lines[i - 1] == new_lines[j - 1] {
            result.push(DiffLine::of(DiffKind::Context, old_lines[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i * stride + (j - 1)] >= dp[(i - 1) * stride + j]) {
            result.push(DiffLine::of(DiffKind::Added, new_lines[j - 1]));
            j -= 1;
        } else {
            result.push(DiffLine::of(DiffKind::Removed, old_lines[i - 1]));
            i -= 1;
        }
    }
    result.reverse();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_new_file() {
        let diff = compute_diff("", "line1\nline2");
        assert_eq!(diff.len(), 2);
        assert!(diff.iter().all(|d| d.kind == DiffKind::Added));
    }

    #[test]
    fn test_diff_modification() {
        let diff = compute_diff(
            "fn main() {\n    println!(\"hello\");\n}",
            "fn main() {\n    println!(\"hello world\");\n    println!(\"goodbye\");\n}",
        );
        assert!(diff.iter().any(|d| d.kind == DiffKind::Added));
        assert!(diff.iter().any(|d| d.kind == DiffKind::Context));
    }

    /// The defect elision exists to fix: a change past the display cap used to
    /// be replaced by 200 unchanged lines, so the user saw no change at all.
    #[test]
    fn change_past_the_cap_is_still_visible() {
        let old: String = (0..300).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line250\n", "CHANGED\n");
        let (shown, _more) = cap_lines(display_diff(&old, &new), DIFF_MAX_LINES);
        assert!(
            shown
                .iter()
                .any(|l| l.kind == DiffKind::Added && l.content == "CHANGED"),
            "the addition must survive the cap: {shown:?}"
        );
        assert!(
            shown
                .iter()
                .any(|l| l.kind == DiffKind::Removed && l.content == "line250"),
            "the removal must survive the cap: {shown:?}"
        );
        assert!(shown.len() <= DIFF_MAX_LINES);
    }

    /// A large file whose middle is entirely rewritten still shows additions,
    /// never 200 removals and nothing else.
    #[test]
    fn wholesale_rewrite_shows_both_sides() {
        let old: String = (0..400).map(|i| format!("old{i}\n")).collect();
        let new: String = (0..400).map(|i| format!("new{i}\n")).collect();
        let (shown, _more) = cap_lines(display_diff(&old, &new), DIFF_MAX_LINES);
        assert!(shown.iter().any(|l| l.kind == DiffKind::Removed));
        assert!(
            shown.iter().any(|l| l.kind == DiffKind::Added),
            "additions must not be hidden behind the removals: {shown:?}"
        );
    }

    #[test]
    fn terminator_only_change_is_named_not_silent() {
        let lines = display_diff("a", "a\n");
        assert!(
            lines
                .iter()
                .any(|l| l.kind == DiffKind::Elided
                    && l.content.contains("trailing newline changed")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(DiffLine::is_change));
    }

    #[test]
    fn crlf_normalisation_is_named_not_silent() {
        let lines = display_diff("a\r\nb\r\n", "a\nb\n");
        assert!(
            lines.iter().any(|l| l.kind == DiffKind::Elided),
            "a CRLF→LF rewrite must not render as an empty diff: {lines:?}"
        );
    }

    #[test]
    fn identical_content_yields_no_lines() {
        assert!(display_diff("a\nb\n", "a\nb\n").is_empty());
    }

    /// An empty real diff is a value, not a sentinel — the whole point of the
    /// enum. An empty original is a new file, and a non-UTF8 original says so.
    #[test]
    fn state_distinguishes_empty_diff_from_missing_capture() {
        assert_eq!(
            WriteDiffState::from_original(b"", ""),
            WriteDiffState::NewFile
        );
        assert_eq!(
            WriteDiffState::from_original(b"a\n", "a\n"),
            WriteDiffState::Diff {
                lines: vec![],
                more: 0
            }
        );
        assert_eq!(
            WriteDiffState::from_original(&[0xff, 0xfe], "x"),
            WriteDiffState::NotCaptured {
                reason: NotCapturedReason::OriginalNotUtf8
            }
        );
    }

    /// A record with no state field predates the feature; it must not claim
    /// the file was new.
    #[test]
    fn default_state_is_unknown_provenance_not_new_file() {
        assert_eq!(
            WriteDiffState::default(),
            WriteDiffState::NotCaptured {
                reason: NotCapturedReason::HistoricalOrReattached
            }
        );
        let restored: WriteDiffState = serde_json::from_str("null").unwrap_or_default();
        assert!(matches!(restored, WriteDiffState::NotCaptured { .. }));
    }

    #[test]
    fn every_reason_has_its_own_parenthetical() {
        let all = [
            NotCapturedReason::NoActiveCheckpoint,
            NotCapturedReason::SnapshotUnavailable,
            NotCapturedReason::SnapshotReadFailed,
            NotCapturedReason::OriginalNotUtf8,
            NotCapturedReason::SupersededInBatch,
            NotCapturedReason::HistoricalOrReattached,
        ];
        let mut seen = std::collections::HashSet::new();
        for r in all {
            assert!(seen.insert(r.describe()), "duplicate text for {r:?}");
        }
        // AC4 / ruling A4 pin this exact parenthetical for the no-checkpoint case.
        assert_eq!(
            NotCapturedReason::NoActiveCheckpoint.describe(),
            "no active checkpoint"
        );
    }

    #[test]
    fn cap_keeps_both_ends_and_reports_the_dropped_count() {
        let lines: Vec<DiffLine> = (0..500)
            .map(|i| DiffLine::of(DiffKind::Added, format!("l{i}")))
            .collect();
        let (shown, more) = cap_lines(lines, DIFF_MAX_LINES);
        assert_eq!(more, 300);
        // 200 content lines plus the inline marker between the two halves.
        assert_eq!(shown.len(), DIFF_MAX_LINES + 1);
        assert_eq!(shown[0].content, "l0", "head is kept");
        assert_eq!(shown[shown.len() - 1].content, "l499", "tail is kept too");
        assert!(
            shown
                .iter()
                .any(|l| l.kind == DiffKind::Elided && l.content == "… 300 more lines"),
            "{shown:?}"
        );
    }
}
