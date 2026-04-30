pub mod virtual_scroll;
pub mod word_wrap;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use std::collections::{BTreeMap, HashMap};

use crate::adapters::tui::state::HeightCache;
use crate::adapters::tui::state::PendingPlanCard;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::feedback_block;
use crate::adapters::tui::widgets::tool_block::{self, ToolBlockState};
use crate::domain::clock::{current_braille_frame, Clock};
use crate::domain::models::{
    ContentBlockType, Conversation, FeedbackBlock, InvocationStatus, MessageRole, PartId, StopReason,
    StreamingState, SummaryTier, ToolCallInfo, ToolResultInfo, Turn, TurnPart, ViewState,
};
use crate::domain::models::turn::tool_call_id_for;
use crate::domain::services::search::SearchMatch;
use crate::domain::services::summary_labeler::compute_summary_label;

use super::empty_state;
use super::plan_card;
use crate::adapters::tui::markdown;
use word_wrap::wrap_text;

/// Result of rendering the chat pane, including boundary data for navigation.
pub struct RenderResult {
    pub total_content_height: usize,
    /// Line offsets (from top) where each content block starts.
    pub block_boundaries: Vec<usize>,
    /// Line offsets (from top) where each message starts (all roles).
    /// Used by the status-bar position counter and by rewind/fork target
    /// resolution — both need a one-to-one mapping between visible turn
    /// anchors and `conversation.messages` indices.
    pub message_boundaries: Vec<usize>,
    /// Line offsets (from top) where each **user** message starts.
    /// Used exclusively by the `{`/`}` keybindings to jump between user
    /// turns, skipping assistant responses.
    pub user_message_boundaries: Vec<usize>,
    /// Tool block id at the top of the viewport (for focus/keyboard interaction).
    pub focused_tool_id: Option<String>,
}

/// Compute the `scroll_offset` value needed to bring `target_message_idx`
/// into the viewport at (or near) the top, using the same offset-from-bottom
/// model as the rest of the TUI scroll code.
///
/// Used by Story 4-4 search navigation (`n` / `N` and calm-jump) and bookmark
/// list "jump to bookmark" to scroll to a specific message. Returns 0 if the
/// message is already in the auto-scroll region (near the bottom).
// Covers: Story 4-4 Task 3.4, Task 5.7
pub fn find_scroll_offset_for_message(
    target_message_idx: usize,
    message_boundaries: &[usize],
    total_content_height: usize,
    viewport_height: usize,
) -> usize {
    if target_message_idx >= message_boundaries.len() {
        return 0;
    }
    let target_line = message_boundaries[target_message_idx];
    let max_offset = total_content_height.saturating_sub(viewport_height);
    if target_line >= max_offset {
        0
    } else {
        max_offset - target_line
    }
}

/// Resolve which message index is currently focused given the scroll state.
///
/// Used by fork, rewind, and bookmark targeting — any feature that says
/// "operate on the message the user can see at the top of the viewport".
/// When `auto_scroll` is true, the focused message is the last one (most
/// recent). Otherwise, a viewport-to-line-to-index binary search is used.
///
/// Returns a value clamped to `[0, message_count - 1]`, so callers can trust
/// the result as a valid index into `conversation.messages`. Returns 0 if
/// `message_count == 0` (caller must separately guard against empty
/// conversations before using the index).
// Covers: Story 4-4 Task 4.0 (extracted from fork/rewind inline patterns)
pub fn find_message_index_from_scroll_offset(
    auto_scroll: bool,
    scroll_offset: usize,
    message_boundaries: &[usize],
    total_content_height: usize,
    viewport_height: usize,
    message_count: usize,
) -> usize {
    if message_count == 0 {
        return 0;
    }
    let last = message_count.saturating_sub(1);
    if auto_scroll {
        return last;
    }
    let max_off = total_content_height.saturating_sub(viewport_height);
    let clamped = scroll_offset.min(max_off);
    let top_line = max_off.saturating_sub(clamped);
    let idx = match message_boundaries.binary_search(&top_line) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    idx.min(last)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod targeting_tests {
    use super::*;

    #[test]
    fn auto_scroll_returns_last_message() {
        let idx = find_message_index_from_scroll_offset(true, 0, &[0, 10, 20], 30, 10, 3);
        assert_eq!(idx, 2);
    }

    #[test]
    fn empty_conversation_returns_zero() {
        let idx = find_message_index_from_scroll_offset(false, 5, &[], 0, 10, 0);
        assert_eq!(idx, 0);
    }

    #[test]
    fn scroll_at_top_returns_first_message() {
        // 3 messages, boundaries at line 0, 10, 20. Total content 30, vp 10.
        // max_off = 20. scroll_offset = 20 means "scrolled to top".
        let idx = find_message_index_from_scroll_offset(false, 20, &[0, 10, 20], 30, 10, 3);
        assert_eq!(idx, 0);
    }

    #[test]
    fn scroll_at_bottom_returns_last_message() {
        let idx = find_message_index_from_scroll_offset(false, 0, &[0, 10, 20], 30, 10, 3);
        assert_eq!(idx, 2);
    }

    #[test]
    fn scroll_mid_returns_middle_message() {
        let idx = find_message_index_from_scroll_offset(false, 10, &[0, 10, 20], 30, 10, 3);
        assert_eq!(idx, 1);
    }

    #[test]
    fn clamps_to_last_when_scroll_offset_oversized() {
        let idx = find_message_index_from_scroll_offset(false, 9999, &[0, 10, 20], 30, 10, 3);
        assert_eq!(idx, 0);
    }

    #[test]
    fn clamps_to_message_count_minus_one() {
        // If message_boundaries has more entries than message_count, we still
        // clamp the returned index.
        let idx = find_message_index_from_scroll_offset(true, 0, &[0, 10, 20, 30], 40, 10, 2);
        assert_eq!(idx, 1);
    }
}

/// Compute the rendered height of a single message (role line + content lines)
/// without building actual Line objects (for off-screen height computation).
///
/// `is_bookmarked`: included as a signature-level contract to mirror
/// `render_message`. The bookmark glyph is prepended to the role line as a
/// 2-column stable-width string (`theme.bookmark_glyph`, validated at theme
/// load to have `unicode-width` ∈ [2, 4]). Because the role label itself is
/// at most `"Assistant:"` (10 chars) and the minimum terminal width enforced
/// by `compute_layout` is 60, the total role-line width of at most 14 cannot
/// wrap. The role line is therefore always 1 row, with or without the
/// bookmark glyph — this parameter asserts that contract rather than
/// computing from it.
#[allow(clippy::too_many_arguments)]
fn compute_message_height(
    content: &str,
    has_error: bool,
    is_cancelled: bool,
    _is_bookmarked: bool,
    width: usize,
) -> usize {
    // 1 for role line (see docstring — role + bookmark glyph never wraps at
    // the enforced minimum width).
    let content_height = if has_error || content.is_empty() {
        let wrapped = wrap_text(content, width);
        wrapped.len()
    } else {
        // Use the markdown pipeline — same code path as render_message() — to
        // guarantee the height invariant required by virtual scrolling (AC6).
        markdown::compute_height(content, width, &markdown::RenderOptions::completed())
    };
    // Cancelled messages append " [interrupted]" as a separate line.
    // compute_height() receives raw content without the suffix, so check if
    // the suffix would push the last rendered line over width. Since
    // render_message() appends it as its own Line, we always add 1.
    let interrupted_line = if is_cancelled { 1 } else { 0 };
    1 + content_height + interrupted_line // role line + content + optional [interrupted]
}

/// Render a single message into Line objects.
///
/// `is_fork_point`: if true, prepend a `🔀` fork marker to the role indicator.
/// Used for the last message in forked conversations (AC3, Story 4-3a).
///
/// `is_bookmarked`: if true, prepend the configured `theme.bookmark_glyph`
/// (default `"» "`) to the role indicator, colored with
/// `theme.colors.bookmark_accent`. Story 4-4 AC9. Stable-width glyph so the
/// height invariant is preserved — see Dev Notes § Bookmark Glyph Theming.
///
/// `search_query`: if `Some`, applies case-insensitive substring highlighting
/// to every match in every content line of the message (Story 4-4 AC2). The
/// highlight uses `theme.search_highlight` for most matches and
/// `theme.search_highlight_focused` for the one at position
/// `focused_match_ordinal_in_message` (0-indexed, scoped to THIS message only).
///
/// Pragmatic v1 note: line-level substring rebuild — for lines containing a
/// match, the non-matched regions lose their original bold/italic/color span
/// styling and fall back to plain text. Acceptable since search-highlight
/// operations are rare and the role line / most content remains untouched.
/// Markdown rendering artifacts (e.g., dropped asterisks from `*bold*`) can
/// cause the rendered match count to diverge from `find_matches` on raw
/// content — known v1 limitation, to be addressed in 4-6 cleanup.
#[allow(clippy::too_many_arguments)]
fn render_message<'a>(
    msg: &crate::domain::models::ChatMessage,
    width: usize,
    theme: &Theme,
    is_fork_point: bool,
    is_bookmarked: bool,
    search_query: Option<&str>,
    focused_match_ordinal_in_message: Option<usize>,
) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    let has_error = msg.content_blocks.contains(&ContentBlockType::Error);

    // Role indicator — may gain a fork marker, a bookmark marker, or both.
    // Fork marker (if any) comes first, then bookmark, then the role label.
    // Keeps order stable so visual tests can match on the prefix sequence.
    let role_text = match msg.role {
        MessageRole::User => "You:",
        MessageRole::Assistant => "Assistant:",
        MessageRole::System => "System:",
    };
    let role_color = match msg.role {
        MessageRole::User => theme.colors.accent,
        MessageRole::Assistant => theme.colors.fg_secondary,
        MessageRole::System => theme.colors.fg_secondary,
    };
    let mut role_spans: Vec<Span<'a>> = Vec::new();
    if is_fork_point {
        role_spans.push(Span::styled(
            "🔀 ".to_string(),
            Style::default().fg(role_color).add_modifier(Modifier::BOLD),
        ));
    }
    if is_bookmarked {
        role_spans.push(Span::styled(
            theme.bookmark_glyph.clone(),
            Style::default()
                .fg(theme.colors.bookmark_accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if msg.synthetic {
        role_spans.push(Span::styled(
            "⤷ ".to_string(),
            Style::default()
                .fg(theme.colors.synthetic_marker)
                .add_modifier(Modifier::BOLD),
        ));
    }
    role_spans.push(Span::styled(
        role_text.to_string(),
        Style::default().fg(role_color).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(role_spans));

    // Content
    if has_error {
        let content_lines = wrap_text(&msg.content, width);
        for text in content_lines {
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(theme.colors.error),
            )));
        }
    } else if msg.content_blocks.contains(&ContentBlockType::PlanSummary) {
        // PlanSummary: thin top border + markdown content
        lines.push(Line::from(Span::styled(
            "┄".repeat(width),
            Style::default().fg(theme.colors.fg_muted),
        )));
        let parsed_lines = markdown::render(
            &msg.content,
            width,
            theme,
            &markdown::RenderOptions::completed(),
        );
        lines.extend(parsed_lines);
    } else {
        let parsed_lines = markdown::render(
            &msg.content,
            width,
            theme,
            &markdown::RenderOptions::completed(),
        );
        lines.extend(parsed_lines);
    }

    // Append [interrupted] suffix for cancelled messages (styled with fg_muted)
    if msg.stop_reason == Some(StopReason::Cancelled) {
        lines.push(Line::from(Span::styled(
            " [interrupted]",
            Style::default().fg(theme.colors.fg_muted),
        )));
    }

    // Apply search highlights if a query is active (Story 4-4 AC2).
    //
    // Second-audit Fix 1: **skip the first line** (the role indicator like
    // "You:" / "Assistant:"). The role line is UI scaffolding, not message
    // content — `find_matches` never scans it, so highlighting it would:
    //   (a) visually highlight the role word if the user queries "assistant",
    //       which looks like a parse error rather than a match, AND
    //   (b) shift `match_cursor` by the number of role-line matches, causing
    //       the focused-match style to land on the wrong content match.
    //
    // Walk the lines in render order starting from index 1, rebuilding each
    // line that contains a match. The match cursor counts every highlight
    // applied across all content lines in this message so the focused-match
    // style lands on the right one.
    if let Some(q) = search_query {
        if !q.is_empty() {
            let mut match_cursor: usize = 0;
            let base_style = theme.search_highlight;
            let focused_style = theme.search_highlight_focused;
            for line in lines.iter_mut().skip(1) {
                *line = apply_search_highlights(
                    line.clone(),
                    q,
                    base_style,
                    focused_style,
                    focused_match_ordinal_in_message,
                    &mut match_cursor,
                );
            }
        }
    }

    lines
}

/// Rebuild `line` with case-insensitive substring highlighting applied to
/// every occurrence of `query`. Non-matched regions fall back to plain
/// unstyled text — see the v1 limitation note on `render_message`.
///
/// `match_cursor` is incremented once per match found; when it equals
/// `focused_idx` at the moment of a match, `focused_style` is applied instead
/// of `base_style`.
fn apply_search_highlights<'a>(
    line: Line<'a>,
    query: &str,
    base_style: Style,
    focused_style: Style,
    focused_idx: Option<usize>,
    match_cursor: &mut usize,
) -> Line<'a> {
    // Flatten the line spans into a single plain string for search.
    let plain: String = line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<Vec<&str>>()
        .concat();

    if plain.is_empty() {
        return line;
    }

    // Build a lowercased mirror with a char-boundary map back to plain bytes,
    // mirroring the approach used by `domain::services::search`.
    let mut lower = String::with_capacity(plain.len());
    let mut boundary_map: Vec<(usize, usize)> = Vec::new();
    for (orig_byte_idx, ch) in plain.char_indices() {
        boundary_map.push((lower.len(), orig_byte_idx));
        for lc in ch.to_lowercase() {
            lower.push(lc);
        }
    }
    boundary_map.push((lower.len(), plain.len()));

    let query_lower: String = query.chars().flat_map(|c| c.to_lowercase()).collect();
    if query_lower.is_empty() || query_lower.len() > lower.len() {
        return line;
    }

    // Collect (orig_start, orig_end) pairs for every match in this line.
    let mut match_ranges: Vec<(usize, usize)> = Vec::new();
    for (mstart_lower, _) in lower.match_indices(query_lower.as_str()) {
        let mend_lower = mstart_lower + query_lower.len();
        let orig_start = boundary_map
            .iter()
            .find(|(l, _)| *l == mstart_lower)
            .map(|(_, o)| *o);
        let orig_end = boundary_map
            .iter()
            .find(|(l, _)| *l == mend_lower)
            .map(|(_, o)| *o);
        if let (Some(s), Some(e)) = (orig_start, orig_end) {
            match_ranges.push((s, e));
        }
    }

    if match_ranges.is_empty() {
        return line;
    }

    // Rebuild the line as a sequence of unstyled (non-match) and styled
    // (match) spans. Non-matched regions lose their original span styling —
    // v1 limitation documented on `render_message`.
    let mut new_spans: Vec<Span<'a>> = Vec::new();
    let mut cursor = 0usize;
    for (ms, me) in &match_ranges {
        if *ms > cursor {
            new_spans.push(Span::raw(plain[cursor..*ms].to_string()));
        }
        let is_focused = focused_idx == Some(*match_cursor);
        let style = if is_focused {
            focused_style
        } else {
            base_style
        };
        new_spans.push(Span::styled(plain[*ms..*me].to_string(), style));
        *match_cursor += 1;
        cursor = *me;
    }
    if cursor < plain.len() {
        new_spans.push(Span::raw(plain[cursor..].to_string()));
    }

    Line::from(new_spans)
}

// ---------------------------------------------------------------------------
// Parts-aware render helpers (Story 16.4)
// ---------------------------------------------------------------------------

/// Effective collapse flag for a turn, combining S16.3's `is_collapsed`
/// overrides with S16.4's content-aware default predicate.
///
/// Order of evaluation:
/// 1. Running turn → always expanded
/// 2. Error invocation → always expanded
/// 3. User-explicit toggle in `view_state.collapsed` → honors it
/// 4. Otherwise → content-aware `default_collapse_predicate`
///
/// Signature: 2 args (`turn`, `view_state`) per P1-3.
fn effective_is_collapsed(turn: &Turn, view_state: &ViewState) -> bool {
    // S16.3 overrides
    if turn.stop_reason.is_none() {
        return false;
    }
    if turn.parts.iter().any(|p| matches!(p, TurnPart::ToolInvocation { status: InvocationStatus::Error, .. })) {
        return false;
    }
    // User-explicit toggle wins
    if let Some(&v) = view_state.collapsed.get(&turn.id) {
        return v;
    }
    // Content-aware predicate
    default_collapse_predicate(turn)
}

/// Content-aware default collapse predicate (S16.3 Q10 carry-forward).
///
/// Rules:
/// - 0–2 tools → always expanded (context, not flood)
/// - ≥3 tools AND tool_lines > prose_lines → collapse (wall of tools)
/// - Otherwise → expanded (prose-dominant)
///
/// `tool_lines` pairs each invocation with its result before measuring
/// height (P0-4 fix — without this every invocation = 1 collapsed line).
/// Uses `ToolBlockState { collapsed: false }` so the predicate sees the
/// true height.
///
/// Signature: 1 arg (`turn`) per P1-3 — frame-stable, reads only immutable
/// turn.parts.
fn default_collapse_predicate(turn: &Turn) -> bool {
    let n = turn.parts.iter().filter(|p| matches!(p, TurnPart::ToolInvocation { .. })).count();
    if n < 3 {
        return false;
    }
    let prose_lines: usize = turn.parts.iter().filter_map(|p| match p {
        TurnPart::Prose { text, .. } | TurnPart::Reasoning { text, .. } => Some(text.matches('\n').count()),
        _ => None,
    }).sum();
    // Pair invocation+result before measuring
    let mut results: std::collections::HashMap<PartId, &TurnPart> = std::collections::HashMap::new();
    for part in &turn.parts {
        if let TurnPart::ToolResult { refs, .. } = part {
            results.insert(*refs, part);
        }
    }
    let tool_lines: usize = turn.parts.iter().filter_map(|p| {
        if let TurnPart::ToolInvocation { id, .. } = p {
            let result_part = results.get(id).copied();
            let tc = adapter_shim(turn, p, result_part);
            Some(tool_block::tool_block_height(&tc, &ToolBlockState { collapsed: false, peek_active: false }))
        } else { None }
    }).sum();
    tool_lines > prose_lines
}

/// Adapt a `TurnPart` into a legacy `ToolCallInfo` for reuse of
/// `tool_block_height` and `render_tool_block_lines`.
///
/// Field mapping follows `rebuild_messages_mirror`'s convention.
/// Uses `tool_call_id_for` (P1-1) so the id format cannot drift.
fn adapter_shim(turn: &Turn, invocation: &TurnPart, result: Option<&TurnPart>) -> ToolCallInfo {
    let (tool, args, status_chip, started_at_ms, ended_at_ms, tool_result, _pid) = match invocation {
        TurnPart::ToolInvocation { id, tool, args, status, started_at, ended_at } => {
            let (chip, result_info) = match status {
                InvocationStatus::Running => (Some("● Executing".to_string()), None),
                InvocationStatus::Success => (Some("✓ Success".to_string()), result.map(|rp| {
                    if let TurnPart::ToolResult { output, .. } = rp {
                        ToolResultInfo { content: output.content.clone(), is_error: output.is_error }
                    } else { ToolResultInfo { content: String::new(), is_error: false } }
                })),
                InvocationStatus::Error => {
                    let ri = result.map(|rp| {
                        if let TurnPart::ToolResult { output, .. } = rp {
                            ToolResultInfo { content: output.content.clone(), is_error: output.is_error }
                        } else { ToolResultInfo { content: String::new(), is_error: true } }
                    });
                    (Some("✗ Error".to_string()), ri)
                }
                InvocationStatus::Cancelled => (Some("⊘ Cancelled".to_string()), None),
                InvocationStatus::Pending => (None, None),
            };
            (tool.clone(), args.clone(), chip, *started_at as u64, ended_at.map(|v| v as u64), result_info, *id)
        }
        _ => (String::new(), serde_json::Value::Null, None, 0u64, None, None, PartId(0)),
    };
    ToolCallInfo {
        id: tool_call_id_for(&turn.id, _pid),
        name: tool,
        input: args,
        result: tool_result,
        started_at_ms: if started_at_ms == 0 { None } else { Some(started_at_ms) },
        completed_at_ms: ended_at_ms,
        status: status_chip,
    }
}

/// Compute the spacing between two consecutive parts within a turn.
///
/// Returns 0 or 1 blank lines per AC3 decision table:
/// - prose/reasoning → tool: 0 (tighter — tool reads as consequence)
/// - tool → prose/reasoning: 1 (standard paragraph spacing)
/// - tool → tool: 0 (flush — tool group)
/// - prose/reasoning → prose/reasoning: 1 (adjacent prose, rare)
fn inter_part_blank_lines(prev: Option<&TurnPart>, next: &TurnPart) -> usize {
    match (prev, next) {
        (None, _) => 0,
        (Some(TurnPart::Prose { .. } | TurnPart::Reasoning { .. }), TurnPart::ToolInvocation { .. } | TurnPart::ToolResult { .. }) => 0,
        (Some(TurnPart::ToolInvocation { .. } | TurnPart::ToolResult { .. }), TurnPart::Prose { .. } | TurnPart::Reasoning { .. }) => 1,
        (Some(TurnPart::ToolInvocation { .. } | TurnPart::ToolResult { .. }), TurnPart::ToolInvocation { .. } | TurnPart::ToolResult { .. }) => 0,
        (Some(TurnPart::Prose { .. } | TurnPart::Reasoning { .. }), TurnPart::Prose { .. } | TurnPart::Reasoning { .. }) => 1,
    }
}

/// Wrap every line in a content vec with the gutter prefix (`│ `) styled accent.
fn gutter_lines<'a>(content_lines: Vec<Line<'a>>, theme: &Theme) -> Vec<Line<'a>> {
    let gutter_style = Style::default().fg(theme.colors.accent);
    content_lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled("│ ", gutter_style)];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Extract the first sentence of a turn's first `Prose` part.
///
/// Sentence boundary is first `.`, `!`, or `?` followed by space or end-of-string.
/// Returns empty string if no Prose part exists.
fn first_prose_sentence(turn: &Turn) -> String {
    let text = turn.parts.iter().find_map(|p| {
        if let TurnPart::Prose { text, .. } = p { Some(text.as_str()) } else { None }
    }).unwrap_or("");
    if text.is_empty() { return String::new(); }
    // Find first sentence boundary: `.`, `!`, or `?` followed by space or end
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?') {
            let next_idx = i + c.len_utf8();
            if next_idx >= text.len() || text.as_bytes()[next_idx] == b' ' {
                return text[..=i].to_string();
            }
        }
    }
    text.to_string()
}

/// Truncate text to `max_width` visual columns, appending `…` (U+2026) if truncated.
///
/// Uses `unicode-width` for column-accurate truncation (P1-8).
fn truncate_with_ellipsis(text: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let vis_width = text.width();
    if vis_width <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    // Truncate char-by-char to fit within the budget (reserve 1 col for ellipsis)
    let ellipsis_width = 1; // U+2026
    let max_text_width = max_width.saturating_sub(ellipsis_width);
    let mut taken = 0usize;
    let mut char_count = 0usize;
    for c in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if taken + cw > max_text_width {
            break;
        }
        taken += cw;
        char_count += c.len_utf8();
    }
    let mut truncated = text[..char_count].to_string();
    truncated.push('…');
    truncated
}

/// Render an assistant turn in expanded form: interleaved parts, gutter wrap,
/// per-invocation rail for running tools.
fn render_expanded_turn<'a>(
    turn: &Turn,
    theme: &'a Theme,
    width: usize,
    clock: &dyn Clock,
    tool_block_states: &HashMap<String, ToolBlockState>,
    liveness: Option<&crate::domain::models::LivenessSnapshot>,
) -> Vec<Line<'a>> {
    let content_width = width.saturating_sub(2); // gutter + space
    if content_width == 0 {
        return vec![];
    }

    // Pre-scan: pair each ToolInvocation with its ToolResult
    let mut result_map: std::collections::HashMap<PartId, &TurnPart> = std::collections::HashMap::new();
    for part in &turn.parts {
        if let TurnPart::ToolResult { refs, .. } = part {
            result_map.insert(*refs, part);
        }
    }

    let mut lines: Vec<Line<'a>> = Vec::new();
    let mut prev: Option<&TurnPart> = None;

    for part in &turn.parts {
        // Inter-part spacing
        let blanks = inter_part_blank_lines(prev, &part);
        for _ in 0..blanks {
            lines.push(Line::from(""));
        }

        match part {
            TurnPart::Prose { text, .. } => {
                let prose_lines = crate::adapters::tui::markdown::render(
                    text, content_width, theme,
                    &crate::adapters::tui::markdown::RenderOptions::completed(),
                );
                lines.extend(prose_lines);
            }
            TurnPart::Reasoning { text, .. } => {
                // P2-2: italic fg_secondary — merge with existing span styles
                let reasoning_lines = crate::adapters::tui::markdown::render(
                    text, content_width, theme,
                    &crate::adapters::tui::markdown::RenderOptions::completed(),
                );
                for line in reasoning_lines {
                    let styled_spans: Vec<Span<'a>> = line.spans.into_iter().map(|s| {
                        let mut style = s.style;
                        style = style.fg(theme.colors.fg_secondary).add_modifier(Modifier::ITALIC);
                        Span::styled(s.content.to_string(), style)
                    }).collect();
                    lines.push(Line::from(styled_spans));
                }
            }
            TurnPart::ToolInvocation { id, tool, status, .. } => {
                let result = result_map.get(id).copied();
                let tc = adapter_shim(turn, part, result);
                let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                let tb_lines = tool_block::render_tool_block_lines(&tc, theme, &tb_state, content_width as u16, clock);
                lines.extend(tb_lines);
                // Per-invocation live rail for running tools only
                if *status == InvocationStatus::Running {
                    let spinner = current_braille_frame(clock);
                    let progress_suffix = liveness
                        .and_then(|l| l.progress)
                        .filter(|_| liveness.as_ref().and_then(|l| l.active_tool_name.as_deref()) == Some(tool.as_str()))
                        .map(|(k, n)| format!(" ({}/{})", k, n))
                        .unwrap_or_default();
                    let rail_spans = vec![
                        Span::styled(spinner.to_string(), Style::default().fg(theme.colors.tool_status_executing)),
                        Span::styled(format!(" {}{}", tool, progress_suffix), Style::default().fg(theme.colors.fg_secondary)),
                    ];
                    lines.push(Line::from(rail_spans));
                }
            }
            TurnPart::ToolResult { .. } => {
                // Already rendered as part of the matching ToolInvocation's
                // expanded view — tool_block reads tc.result to decide shape.
            }
        }

        prev = Some(part);
    }

    // Wrap in gutter and add trailing half-line gap
    let mut result = gutter_lines(lines, theme);
    result.push(Line::from(Span::styled("│", Style::default().fg(theme.colors.accent))));
    result
}

/// Render a collapsed turn as a single gutter-wrapped summary line.
fn render_collapsed_turn<'a>(
    turn: &Turn,
    view_state: &ViewState,
    theme: &'a Theme,
    width: usize,
    clock: &dyn Clock,
) -> Vec<Line<'a>> {
    let sentence = first_prose_sentence(turn);
    let has_error = turn.parts.iter().any(|p| matches!(p, TurnPart::ToolInvocation { status: InvocationStatus::Error, .. }));

    // Compute elapsed from now to first invocation's started_at
    let elapsed_ms: Option<i64> = if turn.stop_reason.is_some() {
        let first_start = turn.parts.iter().find_map(|p| {
            if let TurnPart::ToolInvocation { started_at, .. } = p {
                if *started_at != 0 { Some(*started_at) } else { None }
            } else { None }
        });
        first_start.map(|start| clock.wall_now_ms().saturating_sub(start))
    } else {
        None
    };

    let label = compute_summary_label(turn, elapsed_ms);
    let tier_text = match view_state.summary_tier {
        SummaryTier::Tier1 => &label.tier1,
        SummaryTier::Tier2 => &label.tier2,
    };

    // Build the collapsed line
    use unicode_width::UnicodeWidthStr;
    let collapse_glyph = "▸ ";
    let separator = " · ";
    let success_glyph = "✓";
    let gutter_width = 2; // "│ "
    let separator_width = separator.width(); // " · "
    let success_glyph_width = success_glyph.width(); // "✓"
    let collapse_glyph_width = collapse_glyph.width(); // "▸ "

    let (glyph, glyph_style) = if has_error {
        let failed_name = turn.parts.iter().find_map(|p| {
            if let TurnPart::ToolInvocation { tool, status: InvocationStatus::Error, .. } = p {
                Some(tool.as_str())
            } else { None }
        }).unwrap_or("unknown");
        (format!("✗ {}", failed_name), theme.colors.tool_status_error)
    } else {
        (success_glyph.to_string(), theme.colors.tool_status_success)
    };

    let tier_label_width = tier_text.width(); // visual width, not byte length
    let glyph_width = glyph.width();
    let fixed_width = collapse_glyph_width + separator_width + tier_label_width + glyph_width;

    let collapse_style = Style::default().fg(theme.colors.tool_border_collapsed);
    let muted = Style::default().fg(theme.colors.fg_muted);
    let text_style = Style::default().fg(theme.colors.fg_primary);

    let budget = width.saturating_sub(gutter_width + fixed_width);
    if budget == 0 {
        // Fallback: omit prose, show minimal summary
        let line = Line::from(vec![
            Span::styled(collapse_glyph, collapse_style),
            Span::styled(format!("{}{}", separator, tier_text), muted),
            Span::styled(format!(" {}", glyph), glyph_style),
        ]);
        return gutter_lines(vec![line], theme);
    }

    let truncated = if !sentence.is_empty() {
        truncate_with_ellipsis(&sentence, budget)
    } else {
        String::new()
    };

    let line = if truncated.is_empty() {
        Line::from(vec![
            Span::styled(collapse_glyph, collapse_style),
            Span::styled(format!("{}{}", separator, tier_text), muted),
            Span::styled(format!(" {}", glyph), glyph_style),
        ])
    } else {
        Line::from(vec![
            Span::styled(collapse_glyph, collapse_style),
            Span::styled(format!("{}…", truncated), text_style),
            Span::styled(format!("{}{}", separator, tier_text), muted),
            Span::styled(format!(" {}", glyph), glyph_style),
        ])
    };

    gutter_lines(vec![line], theme)
}

// ---------------------------------------------------------------------------
// Public render API
// ---------------------------------------------------------------------------

/// Render the chat pane with virtual scrolling (viewport culling).
///
/// Returns `RenderResult` with total content height and boundary data.
///
/// Test-only wrapper. Production renders go through `render_with_search`.
/// Delete in S16.10-cleanup alongside the messages mirror.
#[allow(clippy::too_many_arguments, dead_code)]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    streaming: &StreamingState,
    scroll_offset: usize,
    auto_scroll: bool,
    theme: &Theme,
    height_cache: &mut HeightCache,
    tool_block_states: &HashMap<String, ToolBlockState>,
    feedback_blocks: &BTreeMap<String, FeedbackBlock>,
) -> RenderResult {
    render_with_search(
        frame,
        area,
        conversation,
        None,
        streaming,
        &ViewState::default(),
        &crate::domain::clock::SystemClock::default(),
        scroll_offset,
        auto_scroll,
        theme,
        height_cache,
        tool_block_states,
        feedback_blocks,
        None,
        None,
        &[],
        &[],
        None,
    )
}

/// Full chat pane render with optional search highlighting and bookmark
/// marker overlays.
///
/// Story 4-4 AC2: when `search_query` is `Some`, every message is rendered
/// with case-insensitive substring highlights using `theme.search_highlight`.
/// When `focused_search_match` is also `Some`, the matched byte range
/// belonging to the focused-match's message uses the bolder
/// `theme.search_highlight_focused` style so the user can see which match
/// `n` / `N` will move to next.
///
/// Story 4-4 AC9: `bookmarks` is a sorted slice of message indices that
/// should render with a bookmark glyph prefix on their role line. The render
/// loop uses `binary_search` to check membership per message — O(log N) per
/// message, imperceptible even with hundreds of bookmarks.
/// `search_matches`: the full match list for the active query. Used to
/// compute the true focused-match local ordinal when a message contains
/// multiple matches (second-audit Fix 2). Pass `&[]` when no search is
/// active. The event loop owns the canonical list via
/// `state.search_state.matches`.
#[allow(clippy::too_many_arguments)]
pub fn render_with_search(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    streaming: &StreamingState,
    view_state: &ViewState,
    clock: &dyn Clock,
    scroll_offset: usize,
    auto_scroll: bool,
    theme: &Theme,
    height_cache: &mut HeightCache,
    tool_block_states: &HashMap<String, ToolBlockState>,
    feedback_blocks: &BTreeMap<String, FeedbackBlock>,
    search_query: Option<&str>,
    focused_search_match: Option<&SearchMatch>,
    search_matches: &[SearchMatch],
    bookmarks: &[usize],
    pending_plan_card: Option<&PendingPlanCard>,
) -> RenderResult {
    let empty = RenderResult {
        total_content_height: 0,
        block_boundaries: Vec::new(),
        message_boundaries: Vec::new(),
        user_message_boundaries: Vec::new(),
        focused_tool_id: None,
    };

    // Empty state: no messages, no open turn, no streaming, no feedback blocks
    if conversation.messages.is_empty() && open_turn.is_none() && !streaming.is_streaming && feedback_blocks.is_empty() {
        empty_state::render(frame, area, theme);
        return empty;
    }

    let width = area.width as usize;
    if width == 0 {
        return empty;
    }

    let viewport_height = area.height as usize;
    let spacing = theme.spacing.normal as usize;
    let msg_count = conversation.messages.len();

    // Invalidate cache if terminal width changed
    if height_cache.cached_width != area.width {
        height_cache.invalidate_all();
        height_cache.cached_width = area.width;
    }

    // Phase 1: Compute per-message heights and build boundary data.
    // Walk conversation.messages for layout; dispatch on role for height calc.
    let mut message_heights: Vec<usize> = Vec::with_capacity(msg_count + 1);
    let mut block_boundaries: Vec<usize> = Vec::new();
    let mut message_boundaries: Vec<usize> = Vec::new();
    let mut user_message_boundaries: Vec<usize> = Vec::new();
    let mut cumulative_offset: usize = 0;

    for (i, msg) in conversation.messages.iter().enumerate() {
        if i > 0 {
            cumulative_offset += spacing;
        }

        message_boundaries.push(cumulative_offset);
        if msg.role == MessageRole::User {
            user_message_boundaries.push(cumulative_offset);
        }
        block_boundaries.push(cumulative_offset);

        let mut h: usize;
        match msg.role {
            MessageRole::Assistant => {
                // Find corresponding Turn by id (TurnId.0 == ChatMessage.id invariant)
                let turn_opt = conversation.turns.iter().find(|t| t.id.0 == msg.id);
                if let Some(turn) = turn_opt {
                    let collapsed = effective_is_collapsed(turn, view_state);
                    if collapsed {
                        h = 1; // collapsed turn is always 1 line (gutter-wrapped)
                    } else {
                        h = render_expanded_turn(turn, theme, width, clock, tool_block_states, None).len();
                        // Push tool block boundaries for focused-tool navigation:
                        // accumulate line offset through the turn's parts to push a
                        // boundary at each ToolInvocation position.
                        let mut lines_before_tools = 0usize;
                        let mut part_offset = 0usize;
                        for part in &turn.parts {
                            match part {
                                TurnPart::Prose { text, .. } => {
                                    part_offset += crate::adapters::tui::markdown::compute_height(
                                        text, width.saturating_sub(2),
                                        &crate::adapters::tui::markdown::RenderOptions::completed(),
                                    );
                                }
                                TurnPart::Reasoning { text, .. } => {
                                    part_offset += crate::adapters::tui::markdown::compute_height(
                                        text, width.saturating_sub(2),
                                        &crate::adapters::tui::markdown::RenderOptions::completed(),
                                    );
                                }
                                TurnPart::ToolInvocation { .. } => {
                                    block_boundaries.push(cumulative_offset + part_offset);
                                    let tc = adapter_shim(turn, part, None);
                                    part_offset += tool_block::tool_block_height(&tc, &ToolBlockState::default());
                                }
                                TurnPart::ToolResult { .. } => {
                                    // Skip — rendered as part of ToolInvocation
                                }
                            }
                        }
                    }
                } else {
                    // No matching turn — fall back to legacy height calc
                    let has_error = msg.content_blocks.contains(&ContentBlockType::Error);
                    let is_cancelled = msg.stop_reason == Some(StopReason::Cancelled);
                    let is_bookmarked = bookmarks.binary_search(&i).is_ok();
                    h = compute_message_height(&msg.content, has_error, is_cancelled, is_bookmarked, width);
                    for tc in &msg.tool_calls {
                        let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                        h += tool_block::tool_block_height(tc, &tb_state);
                        block_boundaries.push(cumulative_offset + h);
                    }
                }
            }
            MessageRole::User | MessageRole::System => {
                let has_error = msg.content_blocks.contains(&ContentBlockType::Error);
                let is_cancelled = msg.stop_reason == Some(StopReason::Cancelled);
                let is_bookmarked = bookmarks.binary_search(&i).is_ok();
                h = compute_message_height(&msg.content, has_error, is_cancelled, is_bookmarked, width);
                for tc in &msg.tool_calls {
                    let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                    h += tool_block::tool_block_height(tc, &tb_state);
                    block_boundaries.push(cumulative_offset + h);
                }
            }
        }

        // PlanCard heights (Story 6-1a AC5)
        if msg.content_blocks.contains(&ContentBlockType::PlanCard) {
            let mut plans_for_msg: Vec<&crate::domain::models::plan::Plan> = conversation
                .plans
                .values()
                .filter(|p| p.host_message_id.as_deref() == Some(&msg.id))
                .collect();
            plans_for_msg.sort_by_key(|p| p.created_at);
            if plans_for_msg.is_empty() {
                tracing::warn!(
                    "PlanCard block in message {} has no matching plan in conversation.plans",
                    msg.id
                );
                h += plan_card::missing_plan_lines(&msg.id, theme).len();
                block_boundaries.push(cumulative_offset + h);
            } else {
                for plan in &plans_for_msg {
                    let is_pending = pending_plan_card
                        .map(|ppc| ppc.plan_id == plan.id)
                        .unwrap_or(false);
                    h += plan_card::plan_card_height(plan, width as u16, is_pending);
                    block_boundaries.push(cumulative_offset + h);
                }
            }
        }

        if msg.content_blocks.contains(&ContentBlockType::PlanSummary) {
            h += 1;
        }

        // Height cache
        if let Some(cached) = height_cache.get(&msg.id) {
            if cached != h {
                height_cache.invalidate_all();
                height_cache.set(msg.id.clone(), h);
            }
        } else {
            height_cache.set(msg.id.clone(), h);
        }

        message_heights.push(h);
        cumulative_offset += h;
    }

    // Open turn height (live streaming)
    let open_turn_height = if let Some(ot) = open_turn {
        // AC13 suppression check: skip if already in conversation.turns
        let already_committed = conversation.turns.iter().any(|t| t.id == ot.id);
        if already_committed {
            0
        } else {
            if cumulative_offset > 0 {
                cumulative_offset += spacing;
            }
            let h = render_expanded_turn(ot, theme, width, clock, tool_block_states, None).len();
            cumulative_offset += h;
            h
        }
    } else {
        0
    };
    let _ = open_turn_height;

    // Feedback block contribution
    let feedback_pre_height: usize = if !feedback_blocks.is_empty() {
        let pre_spacing = if cumulative_offset > 0 { spacing } else { 0 };
        let fb_heights: usize = feedback_blocks
            .values()
            .map(|fb| feedback_block::render_feedback_lines(fb, area.width, theme).len())
            .sum();
        pre_spacing + fb_heights
    } else {
        0
    };
    let mut total_content_height = cumulative_offset + feedback_pre_height;

    // Phase 2: Determine visible range
    let effective_offset = if auto_scroll {
        0
    } else {
        scroll_offset.min(total_content_height.saturating_sub(viewport_height))
    };

    let visible_start = if total_content_height > viewport_height {
        total_content_height
            .saturating_sub(viewport_height)
            .saturating_sub(effective_offset)
    } else {
        0
    };
    let visible_end = visible_start + viewport_height;

    // Phase 3: Build visible Line objects with role-dispatched render
    let mut lines: Vec<Line> = Vec::new();
    let mut line_offset: usize = 0;

    for (i, msg) in conversation.messages.iter().enumerate() {
        if i > 0 {
            let spacing_end = line_offset + spacing;
            if spacing_end > visible_start && line_offset < visible_end {
                let start = if line_offset >= visible_start { 0 } else { visible_start - line_offset };
                let end = spacing.min(visible_end.saturating_sub(line_offset));
                for _ in start..end {
                    lines.push(Line::from(""));
                }
            }
            line_offset += spacing;
        }

        let msg_height = message_heights[i];
        let msg_end = line_offset + msg_height;

        if msg_end > visible_start && line_offset < visible_end {
            let is_fork_point = conversation.fork_source.is_some()
                && i == conversation.messages.len().saturating_sub(1);
            let focused_local_ordinal: Option<usize> = focused_search_match.and_then(|focused| {
                if focused.message_index != i { return None; }
                search_matches.iter().filter(|m| m.message_index == i).position(|m| m == focused)
            });
            let is_bookmarked = bookmarks.binary_search(&i).is_ok();

            match msg.role {
                MessageRole::Assistant => {
                    let turn_opt = conversation.turns.iter().find(|t| t.id.0 == msg.id);
                    if let Some(turn) = turn_opt {
                        let collapsed = effective_is_collapsed(turn, view_state);
                        let turn_lines = if collapsed {
                            render_collapsed_turn(turn, view_state, theme, width, clock)
                        } else {
                            render_expanded_turn(turn, theme, width, clock, tool_block_states, None)
                        };
                        for (j, line) in turn_lines.into_iter().enumerate() {
                            let abs_line = line_offset + j;
                            if abs_line >= visible_start && abs_line < visible_end {
                                lines.push(line);
                            }
                        }
                    } else {
                        // Legacy fallback for Assistant messages without a Turn
                        let msg_lines = render_message(msg, width, theme, is_fork_point, is_bookmarked, search_query, focused_local_ordinal);
                        let text_height = compute_message_height(&msg.content, msg.content_blocks.contains(&ContentBlockType::Error), msg.stop_reason == Some(StopReason::Cancelled), is_bookmarked, width);
                        for (j, line) in msg_lines.into_iter().enumerate() {
                            let abs_line = line_offset + j;
                            if abs_line >= visible_start && abs_line < visible_end {
                                lines.push(line);
                            }
                        }
                        let mut tool_line_offset = line_offset + text_height;
                        for tc in &msg.tool_calls {
                            let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                            let tb_lines = tool_block::render_tool_block_lines(tc, theme, &tb_state, area.width, clock);
                            for (j, line) in tb_lines.into_iter().enumerate() {
                                let abs_line = tool_line_offset + j;
                                if abs_line >= visible_start && abs_line < visible_end {
                                    lines.push(line);
                                }
                            }
                            tool_line_offset += tool_block::tool_block_height(tc, &tb_state);
                        }
                    }
                }
                MessageRole::User | MessageRole::System => {
                    let msg_lines = render_message(msg, width, theme, is_fork_point, is_bookmarked, search_query, focused_local_ordinal);
                    let text_height = compute_message_height(&msg.content, msg.content_blocks.contains(&ContentBlockType::Error), msg.stop_reason == Some(StopReason::Cancelled), is_bookmarked, width);
                    for (j, line) in msg_lines.into_iter().enumerate() {
                        let abs_line = line_offset + j;
                        if abs_line >= visible_start && abs_line < visible_end {
                            lines.push(line);
                        }
                    }
                    let mut tool_line_offset = line_offset + text_height;
                    for tc in &msg.tool_calls {
                        let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                        let tb_lines = tool_block::render_tool_block_lines(tc, theme, &tb_state, area.width, clock);
                        for (j, line) in tb_lines.into_iter().enumerate() {
                            let abs_line = tool_line_offset + j;
                            if abs_line >= visible_start && abs_line < visible_end {
                                lines.push(line);
                            }
                        }
                        tool_line_offset += tool_block::tool_block_height(tc, &tb_state);
                    }
                }
            }

            // PlanCards for this message
            // TODO(S16.10-cleanup): migrate PlanCard rendering into the
            // parts-aware render path (render_expanded_turn) instead of
            // rendering them at the message level. Currently PlanCards are
            // rendered after the turn content for Assistant messages and
            // after message content for User/System messages.
            if msg.content_blocks.contains(&ContentBlockType::PlanCard) {
                if msg.role == MessageRole::Assistant {
                    tracing::debug!(
                        "PlanCard for Assistant message {} rendered after parts-aware turn content",
                        msg.id
                    );
                }
                let mut plans_for_msg: Vec<&crate::domain::models::plan::Plan> = conversation
                    .plans.values().filter(|p| p.host_message_id.as_deref() == Some(&msg.id)).collect();
                plans_for_msg.sort_by_key(|p| p.created_at);
                let mut tool_line_offset = line_offset + message_heights[i]
                    .saturating_sub(if plans_for_msg.is_empty() { 0 } else { plans_for_msg.iter().map(|p| plan_card::plan_card_height(p, area.width, false)).sum::<usize>() });
                if plans_for_msg.is_empty() {
                    let fallback = plan_card::missing_plan_lines(&msg.id, theme);
                    for (j, line) in fallback.into_iter().enumerate() {
                        let abs_line = tool_line_offset + j;
                        if abs_line >= visible_start && abs_line < visible_end {
                            lines.push(line);
                        }
                    }
                } else {
                    for plan in plans_for_msg {
                        let is_pending = pending_plan_card.map(|ppc| ppc.plan_id == plan.id).unwrap_or(false);
                        let pc_lines = plan_card::render_plan_card_lines(plan, theme, area.width, is_pending);
                        let pc_height = pc_lines.len();
                        for (j, line) in pc_lines.into_iter().enumerate() {
                            let abs_line = tool_line_offset + j;
                            if abs_line >= visible_start && abs_line < visible_end {
                                lines.push(line);
                            }
                        }
                        tool_line_offset += pc_height;
                    }
                }
            }
        }

        line_offset += msg_height;
    }

    // Open turn (live streaming)
    if let Some(ot) = open_turn {
        let already_committed = conversation.turns.iter().any(|t| t.id == ot.id);
        if !already_committed {
            if !conversation.messages.is_empty() {
                let spacing_end = line_offset + spacing;
                if spacing_end > visible_start && line_offset < visible_end {
                    let start = if line_offset >= visible_start { 0 } else { visible_start - line_offset };
                    let end = spacing.min(visible_end.saturating_sub(line_offset));
                    for _ in start..end {
                        lines.push(Line::from(""));
                    }
                }
                line_offset += spacing;
            }
            let turn_lines = render_expanded_turn(ot, theme, width, clock, tool_block_states, None);
            let tl_len = turn_lines.len();
            for (j, line) in turn_lines.into_iter().enumerate() {
                let abs_line = line_offset + j;
                if abs_line >= visible_start && abs_line < visible_end {
                    lines.push(line);
                }
            }
            line_offset += tl_len;
        }
    }

    // Legacy streaming fallback: when open_turn is None but streaming is
    // active, render the old streaming indicator so pre-S16.4 tests and
    // edge-case render-without-turns calls keep working.
    // TODO(S16.10-cleanup): delete this block alongside StreamingState.
    if open_turn.is_none() && streaming.is_streaming {
        if !conversation.messages.is_empty() {
            let spacing_end = line_offset + spacing;
            if spacing_end > visible_start && line_offset < visible_end {
                let start = if line_offset >= visible_start { 0 } else { visible_start - line_offset };
                let end = spacing.min(visible_end.saturating_sub(line_offset));
                for _ in start..end {
                    lines.push(Line::from(""));
                }
            }
            line_offset += spacing;
        }
        if streaming.current_text_buffer.is_empty() {
            if streaming.current_blocks.contains(&ContentBlockType::Error) {
                lines.push(Line::from(Span::styled("Assistant:", Style::default().fg(theme.colors.fg_secondary).add_modifier(Modifier::BOLD))));
                lines.push(Line::from(Span::styled("Error occurred during streaming", Style::default().fg(theme.colors.error))));
            } else {
                lines.push(Line::from(Span::styled("···", Style::default().fg(theme.colors.fg_muted))));
            }
        } else {
            lines.push(Line::from(Span::styled("Assistant:", Style::default().fg(theme.colors.fg_secondary).add_modifier(Modifier::BOLD))));
            let has_error = streaming.current_blocks.contains(&ContentBlockType::Error);
            if has_error {
                let content_lines = crate::adapters::tui::markdown::render(&streaming.current_text_buffer, width, theme, &crate::adapters::tui::markdown::RenderOptions::default());
                for text_line in content_lines {
                    let styled: Vec<Span<'_>> = text_line.spans.into_iter().map(|s| Span::styled(s.content.to_string(), Style::default().fg(theme.colors.error))).collect();
                    lines.push(Line::from(styled));
                }
            } else {
                let parsed_lines = crate::adapters::tui::markdown::render(&streaming.current_text_buffer, width, theme, &crate::adapters::tui::markdown::RenderOptions::default());
                lines.extend(parsed_lines);
            }
        }
    }

    // Feedback blocks at bottom
    if !feedback_blocks.is_empty() {
        if line_offset > 0 {
            let spacing_end = line_offset + spacing;
            if spacing_end > visible_start && line_offset < visible_end {
                let start = if line_offset >= visible_start { 0 } else { visible_start - line_offset };
                let end = spacing.min(visible_end.saturating_sub(line_offset));
                for _ in start..end {
                    lines.push(Line::from(""));
                }
            }
            line_offset += spacing;
        }
        for fb in feedback_blocks.values() {
            let fb_lines = feedback_block::render_feedback_lines(fb, area.width, theme);
            let fb_height = fb_lines.len();
            let fb_end = line_offset + fb_height;
            if fb_end > visible_start && line_offset < visible_end {
                for (j, line) in fb_lines.into_iter().enumerate() {
                    let abs_line = line_offset + j;
                    if abs_line >= visible_start && abs_line < visible_end {
                        lines.push(line);
                    }
                }
            }
            line_offset += fb_height;
            cumulative_offset = line_offset;
        }
    }

    total_content_height = cumulative_offset;

    // Jump-to-bottom indicator
    if !auto_scroll && streaming.is_streaming && !lines.is_empty() {
        let indicator_text = "↓ New content below (streaming) ↓";
        let indicator_len = indicator_text.chars().count();
        let padding = if width > indicator_len { (width - indicator_len) / 2 } else { 0 };
        let centered = format!("{:>width$}", indicator_text, width = padding + indicator_len);
        let indicator_line = Line::from(Span::styled(centered, Style::default().fg(theme.colors.accent).bg(theme.colors.bg_secondary)));
        let last = lines.len() - 1;
        lines[last] = indicator_line;
    }

    let widget = Paragraph::new(Text::from(lines));
    frame.render_widget(widget, area);

    let focused_tool_id = find_focused_tool_id(conversation, streaming, &block_boundaries, visible_start, visible_end);

    RenderResult {
        total_content_height,
        block_boundaries,
        message_boundaries,
        user_message_boundaries,
        focused_tool_id,
    }
}

/// Find the tool block id at the top of the viewport for keyboard focus.
/// Returns the id of the first tool block whose content falls within the top
/// 3 lines of the visible viewport.
fn find_focused_tool_id(
    conversation: &Conversation,
    streaming: &StreamingState,
    block_boundaries: &[usize],
    visible_start: usize,
    _visible_end: usize,
) -> Option<String> {
    // Collect all tool call ids from conversation and streaming
    let all_tool_ids: Vec<String> = conversation
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .chain(streaming.active_tool_calls.values())
        .map(|tc| tc.id.clone())
        .collect();

    if all_tool_ids.is_empty() {
        return None;
    }

    // Find the block boundary closest to visible_start (within 3 lines).
    // Block boundaries include tool block starts — match by index into the
    // tool_ids list (tool blocks are appended to boundaries in order).
    // For MVP: return the first tool id if any boundary is near viewport top.
    for &boundary in block_boundaries {
        if boundary >= visible_start && boundary < visible_start + 3 {
            // A block boundary is at the viewport top.
            // Find the tool id that corresponds to this boundary.
            // Since tool block boundaries are interleaved with message boundaries,
            // we return the first tool call as the focused one.
            return all_tool_ids.into_iter().next();
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Inline tests for parts-aware render helpers (DF-292)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod parts_aware_tests {
    use super::*;
    use crate::domain::clock::MockClock;
    use crate::domain::clock::{BRAILLE_FRAMES, current_braille_frame};

    fn make_prose(text: &str) -> TurnPart {
        TurnPart::Prose { id: PartId(0), text: text.to_string() }
    }

    fn make_reasoning(text: &str) -> TurnPart {
        TurnPart::Reasoning { id: PartId(0), text: text.to_string() }
    }

    fn make_tool(name: &str, status: InvocationStatus) -> TurnPart {
        let is_success = status == InvocationStatus::Success;
        TurnPart::ToolInvocation {
            id: PartId(0), tool: name.to_string(),
            args: serde_json::json!({}), status,
            started_at: 1_700_000_000_000,
            ended_at: if is_success { Some(1_700_000_005_000) } else { None },
        }
    }

    fn make_turn(parts: Vec<TurnPart>, stop_reason: Option<StopReason>) -> Turn {
        let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
        turn.id = crate::domain::models::TurnId("test-turn".into());
        for part in parts {
            turn.push_part(|_id| part);
        }
        turn.stop_reason = stop_reason;
        turn
    }

    // ── AC2 / AC3: gutter_lines and inter_part_blank_lines ──

    #[test]
    fn gutter_lines_prefixes_every_line_with_accent() {
        let theme = Theme::dark();
        let input = vec![Line::from("hello"), Line::from("world")];
        let result = gutter_lines(input, &theme);
        assert_eq!(result.len(), 2);
        for line in &result {
            let combined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(combined.starts_with("│ "), "expected gutter prefix, got: {}", combined);
        }
    }

    #[test]
    fn inter_part_blank_lines_prose_to_tool_is_zero() {
        let prose = make_prose("x");
        let tool = make_tool("Read", InvocationStatus::Success);
        assert_eq!(inter_part_blank_lines(Some(&prose), &tool), 0);
    }

    #[test]
    fn inter_part_blank_lines_tool_to_prose_is_one() {
        let tool = make_tool("Read", InvocationStatus::Success);
        let prose = make_prose("x");
        assert_eq!(inter_part_blank_lines(Some(&tool), &prose), 1);
    }

    #[test]
    fn inter_part_blank_lines_tool_to_tool_is_zero() {
        let t1 = make_tool("Read", InvocationStatus::Success);
        let t2 = make_tool("Bash", InvocationStatus::Running);
        assert_eq!(inter_part_blank_lines(Some(&t1), &t2), 0);
    }

    #[test]
    fn inter_part_blank_lines_prose_to_prose_is_one() {
        let p1 = make_prose("a");
        let p2 = make_prose("b");
        assert_eq!(inter_part_blank_lines(Some(&p1), &p2), 1);
    }

    #[test]
    fn inter_part_blank_lines_none_is_zero() {
        let p = make_prose("x");
        assert_eq!(inter_part_blank_lines(None, &p), 0);
    }

    #[test]
    fn inter_part_blank_lines_reasoning_to_tool_is_zero() {
        let r = make_reasoning("think");
        let t = make_tool("Read", InvocationStatus::Success);
        assert_eq!(inter_part_blank_lines(Some(&r), &t), 0);
    }

    // ── AC8: effective_is_collapsed and default_collapse_predicate ──

    #[test]
    fn default_collapse_one_tool_turn_returns_false() {
        let turn = make_turn(vec![make_prose("x"), make_tool("Read", InvocationStatus::Success)], Some(StopReason::EndTurn));
        assert!(!default_collapse_predicate(&turn));
    }

    #[test]
    fn default_collapse_two_tool_turn_returns_false() {
        let turn = make_turn(vec![
            make_prose("x"),
            make_tool("Read", InvocationStatus::Success),
            make_tool("Grep", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        assert!(!default_collapse_predicate(&turn));
    }

    #[test]
    fn default_collapse_three_tool_dominant_returns_true() {
        // 3 tools, minimal prose → prose_lines = 0, tool_lines > 0 → collapse
        let turn = make_turn(vec![
            make_tool("Read", InvocationStatus::Success),
            make_tool("Grep", InvocationStatus::Success),
            make_tool("Bash", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        assert!(default_collapse_predicate(&turn));
    }

    #[test]
    fn default_collapse_three_tool_prose_dominant_returns_false() {
        // 3 tools, lots of prose → prose dominates, no collapse
        let turn = make_turn(vec![
            make_prose("a\nb\nc\nd\ne\nf\ng\nh\ni\nj"),
            make_tool("Read", InvocationStatus::Success),
            make_tool("Grep", InvocationStatus::Success),
            make_tool("Bash", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        assert!(!default_collapse_predicate(&turn));
    }

    #[test]
    fn effective_is_collapsed_running_turn_returns_false() {
        let turn = make_turn(vec![make_prose("x"), make_tool("Read", InvocationStatus::Running)], None);
        assert!(!effective_is_collapsed(&turn, &ViewState::default()));
    }

    #[test]
    fn effective_is_collapsed_error_invocation_returns_false() {
        let turn = make_turn(vec![
            make_prose("x"),
            make_tool("Bash", InvocationStatus::Error),
        ], Some(StopReason::EndTurn));
        assert!(!effective_is_collapsed(&turn, &ViewState::default()));
    }

    #[test]
    fn effective_is_collapsed_user_explicit_collapsed_returns_true() {
        let turn = make_turn(vec![
            make_prose("long prose here that should dominate"),
            make_prose("more prose content"),
            make_tool("Read", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        let mut vs = ViewState::default();
        vs.collapsed.insert(turn.id.clone(), true);
        assert!(effective_is_collapsed(&turn, &vs));
    }

    #[test]
    fn effective_is_collapsed_user_explicit_expanded_returns_false() {
        let turn = make_turn(vec![
            make_tool("Read", InvocationStatus::Success),
            make_tool("Grep", InvocationStatus::Success),
            make_tool("Bash", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        let mut vs = ViewState::default();
        vs.collapsed.insert(turn.id.clone(), false);
        assert!(!effective_is_collapsed(&turn, &vs));
    }

    #[test]
    fn effective_is_collapsed_predicate_is_frame_stable() {
        let turn = make_turn(vec![
            make_tool("Read", InvocationStatus::Success),
            make_tool("Grep", InvocationStatus::Success),
            make_tool("Bash", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        let vs = ViewState::default();
        let r1 = effective_is_collapsed(&turn, &vs);
        let r2 = effective_is_collapsed(&turn, &vs);
        assert_eq!(r1, r2);
    }

    #[test]
    fn effective_is_collapsed_cancelled_respects_user_collapse() {
        let turn = make_turn(vec![
            make_prose("x"),
            make_tool("Bash", InvocationStatus::Cancelled),
        ], Some(StopReason::EndTurn));
        let mut vs = ViewState::default();
        vs.collapsed.insert(turn.id.clone(), true);
        assert!(effective_is_collapsed(&turn, &vs));
    }

    // ── AC7: truncate_with_ellipsis ──

    #[test]
    fn truncate_with_ellipsis_short_text_no_ellipsis() {
        let result = truncate_with_ellipsis("hello", 80);
        assert_eq!(result, "hello");
        assert!(!result.contains('…'));
    }

    #[test]
    fn truncate_with_ellipsis_long_text_appends_ellipsis() {
        let long = "a".repeat(100);
        let result = truncate_with_ellipsis(&long, 10);
        assert!(result.ends_with('…'));
        use unicode_width::UnicodeWidthStr;
        assert!(result.width() <= 10);
    }

    #[test]
    fn truncate_with_ellipsis_zero_max_width_returns_empty() {
        assert_eq!(truncate_with_ellipsis("x", 0), "");
    }

    #[test]
    fn truncate_with_ellipsis_exact_fit_no_ellipsis() {
        // "abc" has width 3, max=3 → no truncation, no ellipsis
        assert_eq!(truncate_with_ellipsis("abc", 3), "abc");
    }

    // ── AC4: first_prose_sentence ──

    #[test]
    fn first_prose_sentence_finds_period_boundary() {
        let turn = make_turn(vec![make_prose("Hello world. More text.")], Some(StopReason::EndTurn));
        assert_eq!(first_prose_sentence(&turn), "Hello world.");
    }

    #[test]
    fn first_prose_sentence_exclamation_boundary() {
        let turn = make_turn(vec![make_prose("Wow! Indeed.")], Some(StopReason::EndTurn));
        assert_eq!(first_prose_sentence(&turn), "Wow!");
    }

    #[test]
    fn first_prose_sentence_question_boundary() {
        let turn = make_turn(vec![make_prose("Why? Because.")], Some(StopReason::EndTurn));
        assert_eq!(first_prose_sentence(&turn), "Why?");
    }

    #[test]
    fn first_prose_sentence_no_boundary_returns_all() {
        let turn = make_turn(vec![make_prose("no punctuation at all")], Some(StopReason::EndTurn));
        assert_eq!(first_prose_sentence(&turn), "no punctuation at all");
    }

    #[test]
    fn first_prose_sentence_empty_prose_returns_empty() {
        let turn = make_turn(vec![make_prose("")], Some(StopReason::EndTurn));
        assert_eq!(first_prose_sentence(&turn), "");
    }

    #[test]
    fn first_prose_sentence_no_prose_part_returns_empty() {
        let turn = make_turn(vec![make_tool("Read", InvocationStatus::Success)], Some(StopReason::EndTurn));
        assert_eq!(first_prose_sentence(&turn), "");
    }

    // ── AC5: spinner rendering ──

    #[test]
    fn current_braille_frame_cycles_with_clock_frame() {
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let f0 = crate::domain::clock::current_braille_frame(&clock);
        assert_eq!(f0, BRAILLE_FRAMES[0]);
    }

    // ── AC5: running invocation rail rendered, others not ──

    #[test]
    fn running_invocation_renders_rail() {
        let turn = make_turn(vec![
            make_prose("Testing."),
            make_tool("Read", InvocationStatus::Running),
        ], None);
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);
        let text: String = lines.iter().flat_map(|l| l.spans.iter().map(|s| s.content.as_ref())).collect();
        assert!(text.contains("Read"), "expected tool name in output: {}", text);
        let has_spinner = BRAILLE_FRAMES.iter().any(|&f| text.contains(f));
        assert!(has_spinner, "expected spinner glyph: {}", text);
    }

    #[test]
    fn success_invocation_renders_no_rail() {
        let turn = make_turn(vec![
            make_prose("Done."),
            make_tool("Read", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);
        let rail_text: String = lines.iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains("⠋")))
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(rail_text.is_empty(), "expected no spinner rail for success: {}", rail_text);
    }

    // ── AC3: spacing in rendered output ──

    #[test]
    fn spacing_prose_to_tool_is_zero_blank_lines() {
        let turn = make_turn(vec![
            make_prose("Reading file."),
            make_tool("Bash", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);
        // Verify both prose and tool are present, and tool comes after prose
        let all_text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(all_text.contains("Reading file"), "prose not found in: {}", all_text);
        assert!(all_text.contains("Bash"), "tool not found in: {}", all_text);
        let prose_pos = all_text.find("Reading file").unwrap();
        let tool_pos = all_text.find("Bash").unwrap();
        assert!(tool_pos >= prose_pos, "tool before prose: prose={}, tool={}", prose_pos, tool_pos);
    }

    // ── AC10: started_at zero sentinel ──

    #[test]
    fn legacy_invocation_with_started_at_zero_omits_elapsed_in_shim() {
        let mut turn = make_turn(vec![], Some(StopReason::EndTurn));
        // Manually create an invocation with started_at=0 (sentinel)
        turn.parts.push(TurnPart::ToolInvocation {
            id: PartId(0),
            tool: "Read".into(),
            args: serde_json::json!({}),
            status: InvocationStatus::Success,
            started_at: 0,
            ended_at: Some(1),
        });
        let ci = adapter_shim(&turn, &turn.parts[0], None);
        // started_at=0 should map to None (omit elapsed)
        assert_eq!(ci.started_at_ms, None);
    }

    // ── AC2: expanded turn renders gutter ──

    #[test]
    fn expanded_turn_renders_gutter_on_every_line() {
        let turn = make_turn(vec![
            make_prose("Hello."),
            make_tool("Read", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);
        // Every non-empty line should start with the gutter
        for line in &lines {
            if line.spans.is_empty() { continue; }
            let combined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if !combined.trim().is_empty() {
                assert!(combined.starts_with('│'), "missing gutter: {}", combined);
            }
        }
    }

    // ── AC4: collapsed turn renders summary ──

    #[test]
    fn collapsed_turn_renders_summary_line() {
        let turn = make_turn(vec![
            make_prose("Hello world."),
            make_tool("Read", InvocationStatus::Success),
            make_tool("Grep", InvocationStatus::Success),
            make_tool("Bash", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        let mut vs = ViewState::default();
        vs.collapsed.insert(turn.id.clone(), true);
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let lines = render_collapsed_turn(&turn, &vs, &theme, 80, &clock);
        assert_eq!(lines.len(), 1);
        let combined: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(combined.contains('▸'), "missing collapse glyph: {}", combined);
        assert!(combined.contains("tools"), "missing tools label: {}", combined);
    }

    #[test]
    fn collapsed_turn_with_error_shows_error_badge() {
        let turn = make_turn(vec![
            make_prose("Ops."),
            make_tool("Bash", InvocationStatus::Error),
            make_tool("Read", InvocationStatus::Success),
            make_tool("Grep", InvocationStatus::Success),
        ], Some(StopReason::EndTurn));
        let mut vs = ViewState::default();
        vs.collapsed.insert(turn.id.clone(), true);
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let lines = render_collapsed_turn(&turn, &vs, &theme, 80, &clock);
        let combined: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(combined.contains('✗'), "missing error badge: {}", combined);
    }

    // ── Edge cases ──

    #[test]
    fn expanded_turn_zero_width_returns_empty() {
        let turn = make_turn(vec![make_prose("x")], Some(StopReason::EndTurn));
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 0, &clock, &tbs, None);
        assert!(lines.is_empty());
    }
}
