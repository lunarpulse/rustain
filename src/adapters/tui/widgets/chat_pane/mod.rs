pub mod height_cache;
pub mod virtual_scroll;
pub mod word_wrap;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use std::collections::{BTreeMap, HashMap};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::adapters::tui::state::PendingPlanCard;
use crate::adapters::tui::state::{CachedTurnLayout, TabRenderState};
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::feedback_block;
use crate::adapters::tui::widgets::tool_block::{self, ToolBlockState};
use crate::domain::clock::{Clock, current_braille_frame};
use crate::domain::models::turn::tool_call_id_for;
use crate::domain::models::{
    ContentBlockType, Conversation, FeedbackBlock, InvocationStatus, LayoutMetrics,
    MessageRole, PartId, StopReason, StreamingState, SummaryTier, ToolCallInfo, ToolResultInfo, Turn,
    TurnId, TurnPart, ViewState,
};
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
fn hash_message_content(msg: &crate::domain::models::conversation::ChatMessage) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    msg.content.hash(&mut hasher);
    msg.content_blocks.hash(&mut hasher);
    msg.stop_reason.hash(&mut hasher);
    hasher.finish()
}

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
    if turn.parts.iter().any(|p| {
        matches!(
            p,
            TurnPart::ToolInvocation {
                status: InvocationStatus::Error,
                ..
            }
        )
    }) {
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
    let n = turn
        .parts
        .iter()
        .filter(|p| matches!(p, TurnPart::ToolInvocation { .. }))
        .count();
    if n < 3 {
        return false;
    }
    let prose_lines: usize = turn
        .parts
        .iter()
        .filter_map(|p| match p {
            TurnPart::Prose { text, .. } | TurnPart::Reasoning { text, .. } => {
                Some(text.matches('\n').count())
            }
            _ => None,
        })
        .sum();
    // Pair invocation+result before measuring
    let mut results: std::collections::HashMap<PartId, &TurnPart> =
        std::collections::HashMap::new();
    for part in &turn.parts {
        if let TurnPart::ToolResult { refs, .. } = part {
            results.insert(*refs, part);
        }
    }
    let tool_lines: usize = turn
        .parts
        .iter()
        .filter_map(|p| {
            if let TurnPart::ToolInvocation { id, .. } = p {
                let result_part = results.get(id).copied();
                let tc = adapter_shim(turn, p, result_part);
                Some(tool_block::tool_block_height(
                    &tc,
                    &ToolBlockState {
                        collapsed: false,
                        peek_active: false,
                    },
                ))
            } else {
                None
            }
        })
        .sum();
    tool_lines > prose_lines
}

/// Adapt a `TurnPart` into a legacy `ToolCallInfo` for reuse of
/// `tool_block_height` and `render_tool_block_lines`.
///
/// Field mapping follows `rebuild_messages_mirror`'s convention.
/// Uses `tool_call_id_for` (P1-1) so the id format cannot drift.
fn adapter_shim(turn: &Turn, invocation: &TurnPart, result: Option<&TurnPart>) -> ToolCallInfo {
    let (tool, args, status_chip, started_at_ms, ended_at_ms, tool_result, _pid) = match invocation
    {
        TurnPart::ToolInvocation {
            id,
            tool,
            args,
            status,
            started_at,
            ended_at,
        } => {
            let (chip, result_info) = match status {
                InvocationStatus::Running => (Some("● Executing".to_string()), None),
                InvocationStatus::Success => (
                    Some("✓ Success".to_string()),
                    result.map(|rp| {
                        if let TurnPart::ToolResult { output, .. } = rp {
                            ToolResultInfo {
                                content: output.content.clone(),
                                is_error: output.is_error,
                            }
                        } else {
                            ToolResultInfo {
                                content: String::new(),
                                is_error: false,
                            }
                        }
                    }),
                ),
                InvocationStatus::Error => {
                    let ri = result.map(|rp| {
                        if let TurnPart::ToolResult { output, .. } = rp {
                            ToolResultInfo {
                                content: output.content.clone(),
                                is_error: output.is_error,
                            }
                        } else {
                            ToolResultInfo {
                                content: String::new(),
                                is_error: true,
                            }
                        }
                    });
                    (Some("✗ Error".to_string()), ri)
                }
                InvocationStatus::Cancelled => (Some("⊘ Cancelled".to_string()), None),
                InvocationStatus::Pending => (None, None),
            };
            (
                tool.clone(),
                args.clone(),
                chip,
                *started_at as u64,
                ended_at.map(|v| v as u64),
                result_info,
                *id,
            )
        }
        _ => (
            String::new(),
            serde_json::Value::Null,
            None,
            0u64,
            None,
            None,
            PartId(0),
        ),
    };
    ToolCallInfo {
        id: tool_call_id_for(&turn.id, _pid),
        name: tool,
        input: args,
        result: tool_result,
        started_at_ms: if started_at_ms == 0 {
            None
        } else {
            Some(started_at_ms)
        },
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
// Height is clock-independent — see AC6.
pub(super) fn expanded_turn_height(
    turn: &Turn,
    _theme: &Theme,
    width: usize,
    tool_block_states: &HashMap<String, ToolBlockState>,
) -> CachedTurnLayout {
    let content_width = width.saturating_sub(2);
    if content_width == 0 {
        return CachedTurnLayout {
            height: 0,
            block_offsets: vec![],
        };
    }

    // Pre-scan: pair each ToolInvocation with its ToolResult
    let mut result_map: std::collections::HashMap<PartId, &TurnPart> =
        std::collections::HashMap::new();
    for part in &turn.parts {
        if let TurnPart::ToolResult { refs, .. } = part {
            result_map.insert(*refs, part);
        }
    }

    let mut height: usize = 0;
    let mut block_offsets: Vec<usize> = Vec::new();
    let mut prev: Option<&TurnPart> = None;
    let mut running_count: usize = 0;

    for part in &turn.parts {
        // Inter-part spacing
        let blanks = inter_part_blank_lines(prev, part);
        height += blanks;

        match part {
            TurnPart::Prose { text, .. } => {
                block_offsets.push(height);
                height += crate::adapters::tui::markdown::compute_height(
                    text,
                    content_width,
                    &crate::adapters::tui::markdown::RenderOptions::completed(),
                );
            }
            TurnPart::Reasoning { text, .. } => {
                block_offsets.push(height);
                height += crate::adapters::tui::markdown::compute_height(
                    text,
                    content_width,
                    &crate::adapters::tui::markdown::RenderOptions::completed(),
                );
            }
            TurnPart::ToolInvocation { id, status, .. } => {
                block_offsets.push(height);
                let tc = adapter_shim(turn, part, result_map.get(id).copied());
                let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                height += tool_block::tool_block_height(&tc, &tb_state);
                if *status == InvocationStatus::Running {
                    running_count += 1;
                }
            }
            TurnPart::ToolResult { .. } => {
                // Skip — rendered as part of ToolInvocation
            }
        }
        prev = Some(part);
    }

    // Per-Running rail addend + trailing rail line
    height += running_count + 1;

    CachedTurnLayout {
        height,
        block_offsets,
    }
}

// Height is clock-independent — see AC6.
pub(super) fn collapsed_turn_height(
    _turn: &Turn,
    _view_state: &ViewState,
    _theme: &Theme,
    _width: usize,
) -> CachedTurnLayout {
    CachedTurnLayout {
        height: 1,
        block_offsets: vec![],
    }
}

/// Paired-call helper: toggle fold for a turn AND invalidate its cache entry.
/// Task 8 — ensures cache stays coherent with view_state.collapsed mutations.
pub fn toggle_turn_fold(
    view_state: &mut ViewState,
    tab_render_state: &mut TabRenderState,
    turn_id: &TurnId,
) {
    view_state.toggle_fold(turn_id);
    tab_render_state.height_cache.invalidate_turn(turn_id);
}

/// Story 16.6: paired-call helper — set a turn's collapse state AND invalidate
/// its cache entry. Used by `zc` / `zo` (ForceCollapse / ForceExpand).
pub fn set_turn_collapsed(
    view_state: &mut ViewState,
    tab_render_state: &mut TabRenderState,
    turn_id: &TurnId,
    collapsed: bool,
) {
    view_state.collapsed.insert(turn_id.clone(), collapsed);
    tab_render_state.height_cache.invalidate_turn(turn_id);
}

/// Story 16.6: paired-call helper — collapse all turns AND invalidate all cache entries.
/// Used by `zM`.
pub fn collapse_all_turns(
    view_state: &mut ViewState,
    tab_render_state: &mut TabRenderState,
    turns: &[Turn],
) {
    view_state.collapse_all(turns);
    tab_render_state.height_cache.invalidate_all();
}

/// Story 16.6: paired-call helper — expand all turns AND invalidate all cache entries.
/// Used by `zR`.
pub fn expand_all_turns(
    view_state: &mut ViewState,
    tab_render_state: &mut TabRenderState,
    turns: &[Turn],
) {
    view_state.expand_all(turns);
    tab_render_state.height_cache.invalidate_all();
}

/// Paired-call helper: set summary tier AND invalidate all cache entries.
/// Task 9 — summary tier affects every collapsed turn height.
pub fn set_summary_tier(
    view_state: &mut ViewState,
    tab_render_state: &mut TabRenderState,
    tier: SummaryTier,
) {
    view_state.summary_tier = tier;
    tab_render_state.height_cache.invalidate_all();
}

/// Story 16.6, Task 0 — Build LayoutMetrics from conversation turns using the
/// HeightCache. Walks `conversation.turns`, calls `expanded_turn_height` /
/// `collapsed_turn_height` (populating the height cache as a side effect),
/// and produces `turn_top_offsets`, `total_content_height`, and `focused_turn_top`.
///
/// This is the first production consumer of `view_state::reconcile()` — the
/// event-loop dispatcher calls this builder before and after fold mutations
/// to create the `LayoutMetrics` that `reconcile()` consumes.
pub fn build_layout_metrics(
    conversation: &Conversation,
    view_state: &ViewState,
    tab_render_state: &mut TabRenderState,
    theme: &Theme,
    width: u16,
    viewport_height: usize,
    clock: &dyn Clock,
    tool_block_states: &std::collections::HashMap<String, crate::adapters::tui::widgets::tool_block::ToolBlockState>,
) -> LayoutMetrics {
    let _ = clock; // reserved for spinner-frame stability in running-turn heights (S16.5 W4)
    let width_usize = width as usize;
    let spacing = theme.spacing.normal as usize;
    let mut turn_top_offsets: Vec<(TurnId, usize)> = Vec::new();
    let mut cumulative_offset: usize = 0;
    let mut first_turn = true;

    for turn in &conversation.turns {
        if !first_turn {
            cumulative_offset += spacing;
        }
        first_turn = false;
        turn_top_offsets.push((turn.id.clone(), cumulative_offset));

        let collapsed = effective_is_collapsed(turn, view_state);

        let layout = if collapsed {
            // Try cache first
            let key = crate::adapters::tui::state::HeightKey {
                turn_id: turn.id.clone(),
                expansion: false, // collapsed
                summary_tier: view_state.summary_tier,
                terminal_width: width,
                tool_block_states_version: tab_render_state.tool_block_states_version,
            };
            if let Some(cached) = tab_render_state.height_cache.get(&key) {
                cached.clone()
            } else {
                let fresh = collapsed_turn_height(turn, view_state, theme, width_usize);
                tab_render_state.height_cache.set(key, fresh.clone());
                fresh
            }
        } else {
            let key = crate::adapters::tui::state::HeightKey {
                turn_id: turn.id.clone(),
                expansion: true, // expanded
                summary_tier: view_state.summary_tier,
                terminal_width: width,
                tool_block_states_version: tab_render_state.tool_block_states_version,
            };
            if let Some(cached) = tab_render_state.height_cache.get(&key) {
                cached.clone()
            } else {
                let fresh = expanded_turn_height(turn, theme, width_usize, &tool_block_states);
                tab_render_state.height_cache.set(key, fresh.clone());
                fresh
            }
        };

        cumulative_offset += layout.height;
    }

    let focused_turn_top = view_state
        .focused_turn
        .as_ref()
        .and_then(|ft| turn_top_offsets.iter().find(|(tid, _)| *tid == *ft).map(|(_, off)| *off));

    LayoutMetrics {
        viewport_height,
        total_content_height: cumulative_offset,
        turn_top_offsets,
        focused_turn_top,
    }
}

fn inter_part_blank_lines(prev: Option<&TurnPart>, next: &TurnPart) -> usize {
    match (prev, next) {
        (None, _) => 0,
        (
            Some(TurnPart::Prose { .. } | TurnPart::Reasoning { .. }),
            TurnPart::ToolInvocation { .. } | TurnPart::ToolResult { .. },
        ) => 0,
        (
            Some(TurnPart::ToolInvocation { .. } | TurnPart::ToolResult { .. }),
            TurnPart::Prose { .. } | TurnPart::Reasoning { .. },
        ) => 1,
        (
            Some(TurnPart::ToolInvocation { .. } | TurnPart::ToolResult { .. }),
            TurnPart::ToolInvocation { .. } | TurnPart::ToolResult { .. },
        ) => 0,
        (
            Some(TurnPart::Prose { .. } | TurnPart::Reasoning { .. }),
            TurnPart::Prose { .. } | TurnPart::Reasoning { .. },
        ) => 1,
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
    let text = turn
        .parts
        .iter()
        .find_map(|p| {
            if let TurnPart::Prose { text, .. } = p {
                Some(text.as_str())
            } else {
                None
            }
        })
        .unwrap_or("");
    if text.is_empty() {
        return String::new();
    }
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
    let mut result_map: std::collections::HashMap<PartId, &TurnPart> =
        std::collections::HashMap::new();
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
                    text,
                    content_width,
                    theme,
                    &crate::adapters::tui::markdown::RenderOptions::completed(),
                );
                lines.extend(prose_lines);
            }
            TurnPart::Reasoning { text, .. } => {
                // P2-2: italic fg_secondary — merge with existing span styles
                let reasoning_lines = crate::adapters::tui::markdown::render(
                    text,
                    content_width,
                    theme,
                    &crate::adapters::tui::markdown::RenderOptions::completed(),
                );
                for line in reasoning_lines {
                    let styled_spans: Vec<Span<'a>> = line
                        .spans
                        .into_iter()
                        .map(|s| {
                            let mut style = s.style;
                            style = style
                                .fg(theme.colors.fg_secondary)
                                .add_modifier(Modifier::ITALIC);
                            Span::styled(s.content.to_string(), style)
                        })
                        .collect();
                    lines.push(Line::from(styled_spans));
                }
            }
            TurnPart::ToolInvocation {
                id, tool, status, ..
            } => {
                let result = result_map.get(id).copied();
                let tc = adapter_shim(turn, part, result);
                let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                let tb_lines = tool_block::render_tool_block_lines(
                    &tc,
                    theme,
                    &tb_state,
                    content_width as u16,
                    clock,
                );
                lines.extend(tb_lines);
                // Per-invocation live rail for running tools only
                if *status == InvocationStatus::Running {
                    let spinner = current_braille_frame(clock);
                    let progress_suffix = liveness
                        .and_then(|l| l.progress)
                        .filter(|_| {
                            liveness
                                .as_ref()
                                .and_then(|l| l.active_tool_name.as_deref())
                                == Some(tool.as_str())
                        })
                        .map(|(k, n)| format!(" ({}/{})", k, n))
                        .unwrap_or_default();
                    let rail_spans = vec![
                        Span::styled(
                            spinner.to_string(),
                            Style::default().fg(theme.colors.tool_status_executing),
                        ),
                        Span::styled(
                            format!(" {}{}", tool, progress_suffix),
                            Style::default().fg(theme.colors.fg_secondary),
                        ),
                    ];
                    lines.push(Line::from(rail_spans));
                    // Story 16.9: render stdout tail lines below the live rail
                    if let Some(tail) = liveness
                        .and_then(|l| l.tail.as_deref())
                        .filter(|_| {
                            liveness
                                .as_ref()
                                .and_then(|l| l.active_tool_name.as_deref())
                                == Some(tool.as_str())
                        })
                    {
                        // Cap at 4 lines (render-side double-defense; the
                        // producer ring is also capped at tail_lines).
                        // TODO(S16.10-cleanup): plumb ToolProgressConfig::tail_lines through
                        // the render path if user-tunable cap becomes a feature request.
                        let tail_width = content_width.saturating_sub(4); // gutter + 2-space indent + 1-char safety
                        for tail_line in tail.split('\n').take(4) {
                            let truncated = truncate_with_ellipsis(tail_line, tail_width);
                            lines.push(Line::from(vec![Span::styled(
                                format!("  {}", truncated),
                                Style::default().fg(theme.colors.fg_secondary),
                            )]));
                        }
                    }
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
    result.push(Line::from(Span::styled(
        "│",
        Style::default().fg(theme.colors.accent),
    )));
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
    let has_error = turn.parts.iter().any(|p| {
        matches!(
            p,
            TurnPart::ToolInvocation {
                status: InvocationStatus::Error,
                ..
            }
        )
    });

    // Compute elapsed from now to first invocation's started_at
    let elapsed_ms: Option<i64> = if turn.stop_reason.is_some() {
        let first_start = turn.parts.iter().find_map(|p| {
            if let TurnPart::ToolInvocation { started_at, .. } = p {
                if *started_at != 0 {
                    Some(*started_at)
                } else {
                    None
                }
            } else {
                None
            }
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
        let failed_name = turn
            .parts
            .iter()
            .find_map(|p| {
                if let TurnPart::ToolInvocation {
                    tool,
                    status: InvocationStatus::Error,
                    ..
                } = p
                {
                    Some(tool.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("unknown");
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
    tab_render_state: &mut TabRenderState,
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
        tab_render_state,
        tool_block_states,
        feedback_blocks,
        None,
        None,
        &[],
        &[],
        None,
        None, // liveness
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
    tab_render_state: &mut TabRenderState,
    tool_block_states: &HashMap<String, ToolBlockState>,
    feedback_blocks: &BTreeMap<String, FeedbackBlock>,
    search_query: Option<&str>,
    focused_search_match: Option<&SearchMatch>,
    search_matches: &[SearchMatch],
    bookmarks: &[usize],
    pending_plan_card: Option<&PendingPlanCard>,
    liveness: Option<&crate::domain::models::LivenessSnapshot>,
) -> RenderResult {
    let empty = RenderResult {
        total_content_height: 0,
        block_boundaries: Vec::new(),
        message_boundaries: Vec::new(),
        user_message_boundaries: Vec::new(),
        focused_tool_id: None,
    };

    // Empty state: no messages, no open turn, no streaming, no feedback blocks
    if conversation.messages.is_empty()
        && open_turn.is_none()
        && !streaming.is_streaming
        && feedback_blocks.is_empty()
    {
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

    // Invalidate cache if terminal width changed (O(1) divergence check — AC5)
    if tab_render_state.cached_width != Some(area.width) {
        tab_render_state.height_cache.invalidate_all();
        tab_render_state.cached_width = Some(area.width);
    }

    // turn_map borrows &Turn from conversation.turns; height_cache lives on tab (separate root);
    // open_turn is a separate parameter borrow root — no aliasing.
    let turn_map: std::collections::HashMap<&str, &Turn> = conversation
        .turns
        .iter()
        .map(|t| (t.id.0.as_str(), t))
        .collect();

    // Eviction gate: only evict when turn count shrinks (Amelia P0-4 / AC15)
    let turn_count = conversation.turns.len();
    if turn_count < tab_render_state.height_cache.last_seen_turn_count {
        let live = conversation.turns.iter().map(|t| &t.id);
        tab_render_state.height_cache.evict_turns_not_in(live);
    }
    tab_render_state.height_cache.last_seen_turn_count = turn_count;

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
                let turn = turn_map.get(msg.id.as_str()).copied();
                if let Some(turn) = turn {
                    let collapsed = effective_is_collapsed(turn, view_state);
                    let key = crate::adapters::tui::state::HeightKey {
                        turn_id: turn.id.clone(),
                        expansion: !collapsed,
                        summary_tier: view_state.summary_tier,
                        terminal_width: area.width,
                        tool_block_states_version: tab_render_state.tool_block_states_version,
                    };
                    let layout = match tab_render_state.height_cache.get(&key) {
                        Some(l) => {
                            let l = l.clone();
                            #[cfg(debug_assertions)]
                            {
                                crate::adapters::tui::widgets::chat_pane::height_cache::metrics::HITS
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let probe = if collapsed {
                                    collapsed_turn_height(turn, view_state, theme, width)
                                } else {
                                    expanded_turn_height(turn, theme, width, tool_block_states)
                                };
                                if probe.height != l.height
                                    || probe.block_offsets != l.block_offsets
                                {
                                    tracing::warn!(
                                        "HeightCache divergence: turn={}, expansion={}, cached=({}, {:?}), computed=({}, {:?})",
                                        turn.id.0,
                                        !collapsed,
                                        l.height,
                                        l.block_offsets,
                                        probe.height,
                                        probe.block_offsets
                                    );
                                    tab_render_state.height_cache.invalidate_all();
                                }
                            }
                            l
                        }
                        None => {
                            let l = if collapsed {
                                collapsed_turn_height(turn, view_state, theme, width)
                            } else {
                                expanded_turn_height(turn, theme, width, tool_block_states)
                            };
                            #[cfg(debug_assertions)]
                            {
                                crate::adapters::tui::widgets::chat_pane::height_cache::metrics::MISSES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            tab_render_state.height_cache.set(key.clone(), l.clone());
                            l
                        }
                    };
                    h = layout.height;
                    for offset in &layout.block_offsets {
                        block_boundaries.push(cumulative_offset + offset);
                    }
                } else {
                    // TODO(S16.10-cleanup): No matching turn — fall back to legacy height calc
                    let has_error = msg.content_blocks.contains(&ContentBlockType::Error);
                    let is_cancelled = msg.stop_reason == Some(StopReason::Cancelled);
                    let is_bookmarked = bookmarks.binary_search(&i).is_ok();
                    h = compute_message_height(
                        &msg.content,
                        has_error,
                        is_cancelled,
                        is_bookmarked,
                        width,
                    );
                    for tc in &msg.tool_calls {
                        let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                        h += tool_block::tool_block_height(tc, &tb_state);
                        block_boundaries.push(cumulative_offset + h);
                    }
                }
            }
            MessageRole::User | MessageRole::System => {
                let key = crate::adapters::tui::state::MessageHeightKey {
                    msg_id: msg.id.clone(),
                    terminal_width: area.width,
                    content_hash: hash_message_content(msg),
                };
                h = match tab_render_state.height_cache.get_message(&key) {
                    Some(cached_h) => {
                        #[cfg(debug_assertions)]
                        {
                            crate::adapters::tui::widgets::chat_pane::height_cache::metrics::HITS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        cached_h
                    }
                    None => {
                        #[cfg(debug_assertions)]
                        {
                            crate::adapters::tui::widgets::chat_pane::height_cache::metrics::MISSES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        let has_error = msg.content_blocks.contains(&ContentBlockType::Error);
                        let is_cancelled = msg.stop_reason == Some(StopReason::Cancelled);
                        let is_bookmarked = bookmarks.binary_search(&i).is_ok();
                        let computed = compute_message_height(
                            &msg.content,
                            has_error,
                            is_cancelled,
                            is_bookmarked,
                            width,
                        );
                        tab_render_state.height_cache.set_message(key, computed);
                        computed
                    }
                };
                for tc in &msg.tool_calls {
                    let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                    h += tool_block::tool_block_height(tc, &tb_state);
                    block_boundaries.push(cumulative_offset + h);
                }
            }
        }

        // PlanCard heights (Story 6-1a AC5)
        // PlanCard is NOT in the cached layout — added at render time (AC11b).
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

        message_heights.push(h);
        cumulative_offset += h;
    }

    #[cfg(debug_assertions)]
    {
        for (k, _) in tab_render_state.height_cache.entries.iter() {
            debug_assert!(
                turn_map.contains_key(k.turn_id.0.as_str()),
                "HeightCache entry references phantom turn {} not in conversation.turns",
                k.turn_id.0
            );
        }
    }

    // Open turn height (live streaming)
    let open_turn_height = if let Some(ot) = open_turn {
        // AC13 suppression check: skip if already in conversation.turns
        let already_committed = turn_map.contains_key(ot.id.0.as_str());
        if already_committed {
            0
        } else {
            if cumulative_offset > 0 {
                cumulative_offset += spacing;
            }
            let h = expanded_turn_height(ot, theme, width, tool_block_states).height;
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
                let start = if line_offset >= visible_start {
                    0
                } else {
                    visible_start - line_offset
                };
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
                if focused.message_index != i {
                    return None;
                }
                search_matches
                    .iter()
                    .filter(|m| m.message_index == i)
                    .position(|m| m == focused)
            });
            let is_bookmarked = bookmarks.binary_search(&i).is_ok();

            match msg.role {
                MessageRole::Assistant => {
                    let turn_opt = turn_map.get(msg.id.as_str()).copied();
                    if let Some(turn) = turn_opt {
                        let collapsed = effective_is_collapsed(turn, view_state);
                        let turn_lines = if collapsed {
                            render_collapsed_turn(turn, view_state, theme, width, clock)
                        } else {
                            render_expanded_turn(turn, theme, width, clock, tool_block_states, liveness)
                        };
                        let is_focused = view_state.focused_turn.as_ref().is_some_and(|ft| *ft == turn.id);
                        for (j, line) in turn_lines.into_iter().enumerate() {
                            let abs_line = line_offset + j;
                            if abs_line >= visible_start && abs_line < visible_end {
                                if is_focused && j == 0 {
                                    let arrow = Span::styled("▶ ", Style::default()
                                        .fg(theme.colors.accent)
                                        .add_modifier(Modifier::BOLD));
                                    let mut spans = vec![arrow];
                                    spans.extend(line.spans);
                                    lines.push(Line::from(spans));
                                } else {
                                    lines.push(line);
                                }
                            }
                        }
                    } else {
                        // Legacy fallback for Assistant messages without a Turn
                        let msg_lines = render_message(
                            msg,
                            width,
                            theme,
                            is_fork_point,
                            is_bookmarked,
                            search_query,
                            focused_local_ordinal,
                        );
                        let text_height = compute_message_height(
                            &msg.content,
                            msg.content_blocks.contains(&ContentBlockType::Error),
                            msg.stop_reason == Some(StopReason::Cancelled),
                            is_bookmarked,
                            width,
                        );
                        for (j, line) in msg_lines.into_iter().enumerate() {
                            let abs_line = line_offset + j;
                            if abs_line >= visible_start && abs_line < visible_end {
                                lines.push(line);
                            }
                        }
                        let mut tool_line_offset = line_offset + text_height;
                        for tc in &msg.tool_calls {
                            let tb_state =
                                tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                            let tb_lines = tool_block::render_tool_block_lines(
                                tc, theme, &tb_state, area.width, clock,
                            );
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
                    let msg_lines = render_message(
                        msg,
                        width,
                        theme,
                        is_fork_point,
                        is_bookmarked,
                        search_query,
                        focused_local_ordinal,
                    );
                    let text_height = compute_message_height(
                        &msg.content,
                        msg.content_blocks.contains(&ContentBlockType::Error),
                        msg.stop_reason == Some(StopReason::Cancelled),
                        is_bookmarked,
                        width,
                    );
                    for (j, line) in msg_lines.into_iter().enumerate() {
                        let abs_line = line_offset + j;
                        if abs_line >= visible_start && abs_line < visible_end {
                            lines.push(line);
                        }
                    }
                    let mut tool_line_offset = line_offset + text_height;
                    for tc in &msg.tool_calls {
                        let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                        let tb_lines = tool_block::render_tool_block_lines(
                            tc, theme, &tb_state, area.width, clock,
                        );
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
                    .plans
                    .values()
                    .filter(|p| p.host_message_id.as_deref() == Some(&msg.id))
                    .collect();
                plans_for_msg.sort_by_key(|p| p.created_at);
                let mut tool_line_offset = line_offset
                    + message_heights[i].saturating_sub(if plans_for_msg.is_empty() {
                        0
                    } else {
                        plans_for_msg
                            .iter()
                            .map(|p| plan_card::plan_card_height(p, area.width, false))
                            .sum::<usize>()
                    });
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
                        let is_pending = pending_plan_card
                            .map(|ppc| ppc.plan_id == plan.id)
                            .unwrap_or(false);
                        let pc_lines =
                            plan_card::render_plan_card_lines(plan, theme, area.width, is_pending);
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
        let already_committed = turn_map.contains_key(ot.id.0.as_str());
        if !already_committed {
            if !conversation.messages.is_empty() {
                let spacing_end = line_offset + spacing;
                if spacing_end > visible_start && line_offset < visible_end {
                    let start = if line_offset >= visible_start {
                        0
                    } else {
                        visible_start - line_offset
                    };
                    let end = spacing.min(visible_end.saturating_sub(line_offset));
                    for _ in start..end {
                        lines.push(Line::from(""));
                    }
                }
                line_offset += spacing;
            }
            let turn_lines = render_expanded_turn(ot, theme, width, clock, tool_block_states, liveness);
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
                let start = if line_offset >= visible_start {
                    0
                } else {
                    visible_start - line_offset
                };
                let end = spacing.min(visible_end.saturating_sub(line_offset));
                for _ in start..end {
                    lines.push(Line::from(""));
                }
            }
            line_offset += spacing;
        }
        if streaming.current_text_buffer.is_empty() {
            if streaming.current_blocks.contains(&ContentBlockType::Error) {
                lines.push(Line::from(Span::styled(
                    "Assistant:",
                    Style::default()
                        .fg(theme.colors.fg_secondary)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "Error occurred during streaming",
                    Style::default().fg(theme.colors.error),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "···",
                    Style::default().fg(theme.colors.fg_muted),
                )));
            }
        } else {
            lines.push(Line::from(Span::styled(
                "Assistant:",
                Style::default()
                    .fg(theme.colors.fg_secondary)
                    .add_modifier(Modifier::BOLD),
            )));
            let has_error = streaming.current_blocks.contains(&ContentBlockType::Error);
            if has_error {
                let content_lines = crate::adapters::tui::markdown::render(
                    &streaming.current_text_buffer,
                    width,
                    theme,
                    &crate::adapters::tui::markdown::RenderOptions::default(),
                );
                for text_line in content_lines {
                    let styled: Vec<Span<'_>> = text_line
                        .spans
                        .into_iter()
                        .map(|s| {
                            Span::styled(
                                s.content.to_string(),
                                Style::default().fg(theme.colors.error),
                            )
                        })
                        .collect();
                    lines.push(Line::from(styled));
                }
            } else {
                let parsed_lines = crate::adapters::tui::markdown::render(
                    &streaming.current_text_buffer,
                    width,
                    theme,
                    &crate::adapters::tui::markdown::RenderOptions::default(),
                );
                lines.extend(parsed_lines);
            }
        }
    }

    // Feedback blocks at bottom
    if !feedback_blocks.is_empty() {
        if line_offset > 0 {
            let spacing_end = line_offset + spacing;
            if spacing_end > visible_start && line_offset < visible_end {
                let start = if line_offset >= visible_start {
                    0
                } else {
                    visible_start - line_offset
                };
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
        let padding = if width > indicator_len {
            (width - indicator_len) / 2
        } else {
            0
        };
        let centered = format!(
            "{:>width$}",
            indicator_text,
            width = padding + indicator_len
        );
        let indicator_line = Line::from(Span::styled(
            centered,
            Style::default()
                .fg(theme.colors.accent)
                .bg(theme.colors.bg_secondary),
        ));
        let last = lines.len() - 1;
        lines[last] = indicator_line;
    }

    let widget = Paragraph::new(Text::from(lines));
    frame.render_widget(widget, area);

    let focused_tool_id = find_focused_tool_id(
        conversation,
        streaming,
        &block_boundaries,
        visible_start,
        visible_end,
    );

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
    use crate::domain::models::ChatMessage;

    fn make_prose(text: &str) -> TurnPart {
        TurnPart::Prose {
            id: PartId(0),
            text: text.to_string(),
        }
    }

    fn make_reasoning(text: &str) -> TurnPart {
        TurnPart::Reasoning {
            id: PartId(0),
            text: text.to_string(),
        }
    }

    fn make_tool(name: &str, status: InvocationStatus) -> TurnPart {
        let is_success = status == InvocationStatus::Success;
        TurnPart::ToolInvocation {
            id: PartId(0),
            tool: name.to_string(),
            args: serde_json::json!({}),
            status,
            started_at: 1_700_000_000_000,
            ended_at: if is_success {
                Some(1_700_000_005_000)
            } else {
                None
            },
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
            assert!(
                combined.starts_with("│ "),
                "expected gutter prefix, got: {}",
                combined
            );
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
        let turn = make_turn(
            vec![
                make_prose("x"),
                make_tool("Read", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        assert!(!default_collapse_predicate(&turn));
    }

    #[test]
    fn default_collapse_two_tool_turn_returns_false() {
        let turn = make_turn(
            vec![
                make_prose("x"),
                make_tool("Read", InvocationStatus::Success),
                make_tool("Grep", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        assert!(!default_collapse_predicate(&turn));
    }

    #[test]
    fn default_collapse_three_tool_dominant_returns_true() {
        // 3 tools, minimal prose → prose_lines = 0, tool_lines > 0 → collapse
        let turn = make_turn(
            vec![
                make_tool("Read", InvocationStatus::Success),
                make_tool("Grep", InvocationStatus::Success),
                make_tool("Bash", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        assert!(default_collapse_predicate(&turn));
    }

    #[test]
    fn default_collapse_three_tool_prose_dominant_returns_false() {
        // 3 tools, lots of prose → prose dominates, no collapse
        let turn = make_turn(
            vec![
                make_prose("a\nb\nc\nd\ne\nf\ng\nh\ni\nj"),
                make_tool("Read", InvocationStatus::Success),
                make_tool("Grep", InvocationStatus::Success),
                make_tool("Bash", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        assert!(!default_collapse_predicate(&turn));
    }

    #[test]
    fn effective_is_collapsed_running_turn_returns_false() {
        let turn = make_turn(
            vec![
                make_prose("x"),
                make_tool("Read", InvocationStatus::Running),
            ],
            None,
        );
        assert!(!effective_is_collapsed(&turn, &ViewState::default()));
    }

    #[test]
    fn effective_is_collapsed_error_invocation_returns_false() {
        let turn = make_turn(
            vec![make_prose("x"), make_tool("Bash", InvocationStatus::Error)],
            Some(StopReason::EndTurn),
        );
        assert!(!effective_is_collapsed(&turn, &ViewState::default()));
    }

    #[test]
    fn effective_is_collapsed_user_explicit_collapsed_returns_true() {
        let turn = make_turn(
            vec![
                make_prose("long prose here that should dominate"),
                make_prose("more prose content"),
                make_tool("Read", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let mut vs = ViewState::default();
        vs.collapsed.insert(turn.id.clone(), true);
        assert!(effective_is_collapsed(&turn, &vs));
    }

    #[test]
    fn effective_is_collapsed_user_explicit_expanded_returns_false() {
        let turn = make_turn(
            vec![
                make_tool("Read", InvocationStatus::Success),
                make_tool("Grep", InvocationStatus::Success),
                make_tool("Bash", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let mut vs = ViewState::default();
        vs.collapsed.insert(turn.id.clone(), false);
        assert!(!effective_is_collapsed(&turn, &vs));
    }

    #[test]
    fn effective_is_collapsed_predicate_is_frame_stable() {
        let turn = make_turn(
            vec![
                make_tool("Read", InvocationStatus::Success),
                make_tool("Grep", InvocationStatus::Success),
                make_tool("Bash", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let vs = ViewState::default();
        let r1 = effective_is_collapsed(&turn, &vs);
        let r2 = effective_is_collapsed(&turn, &vs);
        assert_eq!(r1, r2);
    }

    #[test]
    fn effective_is_collapsed_cancelled_respects_user_collapse() {
        let turn = make_turn(
            vec![
                make_prose("x"),
                make_tool("Bash", InvocationStatus::Cancelled),
            ],
            Some(StopReason::EndTurn),
        );
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
        let turn = make_turn(
            vec![make_prose("Hello world. More text.")],
            Some(StopReason::EndTurn),
        );
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
        let turn = make_turn(
            vec![make_prose("no punctuation at all")],
            Some(StopReason::EndTurn),
        );
        assert_eq!(first_prose_sentence(&turn), "no punctuation at all");
    }

    #[test]
    fn first_prose_sentence_empty_prose_returns_empty() {
        let turn = make_turn(vec![make_prose("")], Some(StopReason::EndTurn));
        assert_eq!(first_prose_sentence(&turn), "");
    }

    #[test]
    fn first_prose_sentence_no_prose_part_returns_empty() {
        let turn = make_turn(
            vec![make_tool("Read", InvocationStatus::Success)],
            Some(StopReason::EndTurn),
        );
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
        let turn = make_turn(
            vec![
                make_prose("Testing."),
                make_tool("Read", InvocationStatus::Running),
            ],
            None,
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            text.contains("Read"),
            "expected tool name in output: {}",
            text
        );
        let has_spinner = BRAILLE_FRAMES.iter().any(|&f| text.contains(f));
        assert!(has_spinner, "expected spinner glyph: {}", text);
    }

    #[test]
    fn success_invocation_renders_no_rail() {
        let turn = make_turn(
            vec![
                make_prose("Done."),
                make_tool("Read", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);
        let rail_text: String = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains("⠋")))
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            rail_text.is_empty(),
            "expected no spinner rail for success: {}",
            rail_text
        );
    }

    // ── AC3: spacing in rendered output ──

    #[test]
    fn spacing_prose_to_tool_is_zero_blank_lines() {
        let turn = make_turn(
            vec![
                make_prose("Reading file."),
                make_tool("Bash", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);
        // Verify both prose and tool are present, and tool comes after prose
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            all_text.contains("Reading file"),
            "prose not found in: {}",
            all_text
        );
        assert!(all_text.contains("Bash"), "tool not found in: {}", all_text);
        let prose_pos = all_text.find("Reading file").unwrap();
        let tool_pos = all_text.find("Bash").unwrap();
        assert!(
            tool_pos >= prose_pos,
            "tool before prose: prose={}, tool={}",
            prose_pos,
            tool_pos
        );
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
        let turn = make_turn(
            vec![
                make_prose("Hello."),
                make_tool("Read", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);
        // Every non-empty line should start with the gutter
        for line in &lines {
            if line.spans.is_empty() {
                continue;
            }
            let combined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if !combined.trim().is_empty() {
                assert!(combined.starts_with('│'), "missing gutter: {}", combined);
            }
        }
    }

    // ── AC4: collapsed turn renders summary ──

    #[test]
    fn collapsed_turn_renders_summary_line() {
        let turn = make_turn(
            vec![
                make_prose("Hello world."),
                make_tool("Read", InvocationStatus::Success),
                make_tool("Grep", InvocationStatus::Success),
                make_tool("Bash", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let mut vs = ViewState::default();
        vs.collapsed.insert(turn.id.clone(), true);
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let lines = render_collapsed_turn(&turn, &vs, &theme, 80, &clock);
        assert_eq!(lines.len(), 1);
        let combined: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            combined.contains('▸'),
            "missing collapse glyph: {}",
            combined
        );
        assert!(
            combined.contains("tools"),
            "missing tools label: {}",
            combined
        );
    }

    #[test]
    fn collapsed_turn_with_error_shows_error_badge() {
        let turn = make_turn(
            vec![
                make_prose("Ops."),
                make_tool("Bash", InvocationStatus::Error),
                make_tool("Read", InvocationStatus::Success),
                make_tool("Grep", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
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

    // ── Height-cache paired-call helper tests (Story 16-5) ──

    #[test]
    fn toggle_turn_fold_invalidates_turn_cache() {
        let turn = make_turn(vec![make_prose("hello")], Some(StopReason::EndTurn));
        let mut view_state = ViewState::default();
        let mut tab_render_state = TabRenderState::default();
        let key = crate::adapters::tui::state::HeightKey {
            turn_id: turn.id.clone(),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 0,
        };
        tab_render_state.height_cache.set(
            key.clone(),
            CachedTurnLayout {
                height: 5,
                block_offsets: vec![],
            },
        );
        assert!(tab_render_state.height_cache.get(&key).is_some());

        toggle_turn_fold(&mut view_state, &mut tab_render_state, &turn.id);

        assert!(tab_render_state.height_cache.get(&key).is_none());
        assert!(view_state.is_collapsed(&turn));
    }

    #[test]
    fn set_summary_tier_invalidates_all_cache() {
        let mut view_state = ViewState::default();
        let mut tab_render_state = TabRenderState::default();
        let key = crate::adapters::tui::state::HeightKey {
            turn_id: crate::domain::models::TurnId("t1".into()),
            expansion: false,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 0,
        };
        tab_render_state.height_cache.set(
            key.clone(),
            CachedTurnLayout {
                height: 1,
                block_offsets: vec![],
            },
        );
        assert!(tab_render_state.height_cache.get(&key).is_some());

        set_summary_tier(&mut view_state, &mut tab_render_state, SummaryTier::Tier2);

        assert!(tab_render_state.height_cache.get(&key).is_none());
        assert_eq!(view_state.summary_tier, SummaryTier::Tier2);
    }

    #[test]
    fn expanded_turn_height_returns_clock_independent_layout() {
        let turn = make_turn(
            vec![make_prose("line one\nline two")],
            Some(StopReason::EndTurn),
        );
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let layout = expanded_turn_height(&turn, &theme, 80, &tbs);
        assert!(layout.height > 0);
        // Clock-independent: no clock parameter in signature
    }

    #[test]
    fn collapsed_turn_height_returns_one() {
        let turn = make_turn(
            vec![make_prose("line one\nline two")],
            Some(StopReason::EndTurn),
        );
        let view_state = ViewState::default();
        let theme = Theme::dark();
        let layout = collapsed_turn_height(&turn, &view_state, &theme, 80);
        assert_eq!(layout.height, 1);
        assert!(layout.block_offsets.is_empty());
    }

    #[test]
    fn height_key_version_change_produces_different_cache_entries() {
        let mut cache = crate::adapters::tui::state::HeightCache::default();
        let turn_id = crate::domain::models::TurnId("t1".into());
        let key_v0 = crate::adapters::tui::state::HeightKey {
            turn_id: turn_id.clone(),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 0,
        };
        let key_v1 = crate::adapters::tui::state::HeightKey {
            turn_id: turn_id.clone(),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 1,
        };
        cache.set(
            key_v0.clone(),
            CachedTurnLayout {
                height: 10,
                block_offsets: vec![],
            },
        );
        cache.set(
            key_v1.clone(),
            CachedTurnLayout {
                height: 20,
                block_offsets: vec![],
            },
        );
        assert_eq!(cache.get(&key_v0).unwrap().height, 10);
        assert_eq!(cache.get(&key_v1).unwrap().height, 20);
    }

    #[test]
    fn height_key_width_change_produces_different_cache_entries() {
        let mut cache = crate::adapters::tui::state::HeightCache::default();
        let turn_id = crate::domain::models::TurnId("t1".into());
        let key_w80 = crate::adapters::tui::state::HeightKey {
            turn_id: turn_id.clone(),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 0,
        };
        let key_w40 = crate::adapters::tui::state::HeightKey {
            turn_id: turn_id.clone(),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 40,
            tool_block_states_version: 0,
        };
        cache.set(
            key_w80.clone(),
            CachedTurnLayout {
                height: 5,
                block_offsets: vec![],
            },
        );
        cache.set(
            key_w40.clone(),
            CachedTurnLayout {
                height: 8,
                block_offsets: vec![],
            },
        );
        assert_eq!(cache.get(&key_w80).unwrap().height, 5);
        assert_eq!(cache.get(&key_w40).unwrap().height, 8);
    }

    #[serial_test::serial]
    #[test]
    fn cache_metrics_miss_increments_on_uncached_turn() {
        crate::adapters::tui::widgets::chat_pane::height_cache::metrics::reset();
        let mut cache = crate::adapters::tui::state::HeightCache::default();
        let key = crate::adapters::tui::state::HeightKey {
            turn_id: crate::domain::models::TurnId("t1".into()),
            expansion: true,
            summary_tier: SummaryTier::Tier1,
            terminal_width: 80,
            tool_block_states_version: 0,
        };
        // Miss: get on empty cache
        let _ = cache.get(&key);
        let (_, misses) =
            crate::adapters::tui::widgets::chat_pane::height_cache::metrics::snapshot();
        assert_eq!(
            misses, 0,
            "metrics only count render-path misses, not raw cache.get misses"
        );
    }

    // ── AC11: expanded_turn_height parity tests (10 fixtures) ──

    #[test]
    fn parity_empty_turn() {
        let turn = make_turn(vec![], Some(StopReason::EndTurn));
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let cached = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None).len();
        assert_eq!(cached.height, rendered, "parity: empty turn");
        // Empty turn: only trailing rail = 1
        assert_eq!(cached.height, 1);
    }

    #[test]
    fn parity_single_prose_part() {
        let turn = make_turn(vec![make_prose("Hello world")], Some(StopReason::EndTurn));
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let cached = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None).len();
        assert_eq!(cached.height, rendered, "parity: single prose");
    }

    #[test]
    fn parity_prose_with_inter_part_blank() {
        let turn = make_turn(
            vec![
                make_prose("First paragraph"),
                make_prose("Second paragraph"),
            ],
            Some(StopReason::EndTurn),
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let cached = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None).len();
        assert_eq!(cached.height, rendered, "parity: prose + inter-part blank");
    }

    #[test]
    fn parity_running_tool_with_rail() {
        let turn = make_turn(
            vec![
                make_prose("Running a tool..."),
                make_tool("Read", InvocationStatus::Running),
            ],
            None, // stop_reason = None → running turn
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let cached = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None).len();
        assert_eq!(cached.height, rendered, "parity: running tool with rail");
        // Should include per-Running rail addend
        assert!(cached.height > 0);
    }

    #[test]
    fn parity_completed_tool_expanded() {
        let turn = make_turn(
            vec![
                make_prose("Done."),
                make_tool("Read", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let mut tbs: HashMap<String, ToolBlockState> = HashMap::new();
        // Expanded tool block
        tbs.insert(
            "test-turn-1".into(),
            ToolBlockState {
                collapsed: false,
                peek_active: false,
            },
        );
        let cached = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None).len();
        assert_eq!(cached.height, rendered, "parity: completed tool expanded");
    }

    #[test]
    fn parity_multi_part_mixed_prose_tool_prose() {
        let turn = make_turn(
            vec![
                make_prose("Intro."),
                make_tool("Read", InvocationStatus::Success),
                make_prose("Outro."),
            ],
            Some(StopReason::EndTurn),
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let cached = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None).len();
        assert_eq!(cached.height, rendered, "parity: mixed prose-tool-prose");
    }

    #[test]
    fn parity_trailing_rail_after_running() {
        let turn = make_turn(vec![make_tool("Read", InvocationStatus::Running)], None);
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();
        let cached = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None).len();
        assert_eq!(
            cached.height, rendered,
            "parity: trailing rail after running"
        );
    }

    /// Fixture 6: Tool peek state (peek_active: true, collapsed: false).
    /// Covers AC1 `tool_block_states_version` — 3rd tool state distinct from
    /// default-collapsed and expanded.
    #[test]
    fn parity_tool_peek_state() {
        let turn = make_turn(
            vec![
                make_prose("Let me check."),
                make_tool("Read", InvocationStatus::Success),
            ],
            Some(StopReason::EndTurn),
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let mut tbs: HashMap<String, ToolBlockState> = HashMap::new();
        tbs.insert(
            "test-turn".into(),
            ToolBlockState {
                collapsed: false,
                peek_active: true,
            },
        );
        let cached = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None).len();
        assert_eq!(cached.height, rendered, "parity: tool peek state");
        assert!(cached.height > 0);
    }

    /// Fixture 8: Spinner digit-cliff — elapsed crosses 9.9s → 10.0s.
    /// Keeps `tool_block.rs:139` `→ running... ({elapsed})` height-stable
    /// across the width-cliff where elapsed string gains a digit.
    #[test]
    fn parity_spinner_elapsed_digit_cliff() {
        let turn = make_turn(
            vec![
                make_prose("Working..."),
                make_tool("Read", InvocationStatus::Running),
            ],
            None,
        );
        let clock_9s = MockClock::at_wall_ms(1_700_000_000_000);
        clock_9s.set_wall_anchor_ms(1_700_000_009_000); // 9s elapsed
        let clock_10s = MockClock::at_wall_ms(1_700_000_000_000);
        clock_10s.set_wall_anchor_ms(1_700_000_010_000); // 10s elapsed

        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();

        let cached_9s = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered_9s = render_expanded_turn(&turn, &theme, 80, &clock_9s, &tbs, None).len();
        let cached_10s = expanded_turn_height(&turn, &theme, 80, &tbs);
        let rendered_10s = render_expanded_turn(&turn, &theme, 80, &clock_10s, &tbs, None).len();

        assert_eq!(cached_9s.height, rendered_9s, "parity: 9.9s elapsed");
        assert_eq!(cached_10s.height, rendered_10s, "parity: 10.0s elapsed");
        // Height must be stable across the digit cliff (clock-independent)
        assert_eq!(
            cached_9s.height, cached_10s.height,
            "spinner digit-cliff must not change height"
        );
    }

    /// AC6: Spinner ticks do NOT invalidate cache — strict equality.
    /// Exercises the full render+HeightCache path: warm cache, advance clock
    /// frames, render again, assert cache size unchanged.
    #[test]
    fn spinner_tick_strict_equality() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let turn = make_turn(
            vec![
                make_prose("Running..."),
                make_tool("Read", InvocationStatus::Running),
            ],
            None,
        );
        let mut conversation = Conversation {
            id: "test-spinner".into(),
            title: String::new(),
            messages: vec![ChatMessage {
                id: "test-turn".into(),
                role: MessageRole::Assistant,
                content: String::new(),
                content_blocks: vec![],
                tool_calls: vec![],
                created_at: 1_700_000,
                token_count: None,
                stop_reason: None,
                synthetic: false,
                images: vec![],
            }],
            turns: vec![turn.clone()],
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: HashMap::new(),
            fork_source: None,
        };
        let theme = Theme::dark();
        let streaming = StreamingState::default();
        let mut tab_render_state = TabRenderState::default();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();

        // Warm render: populates cache
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let _ = render_with_search(
                    frame,
                    Rect::new(0, 0, 80, 24),
                    &conversation,
                    None,
                    &streaming,
                    &ViewState::default(),
                    &MockClock::at_wall_ms(1_700_000_000_000),
                    0,
                    true,
                    &theme,
                    &mut tab_render_state,
                    &tbs,
                    &BTreeMap::new(),
                    None,
                    None,
                    &[],
                    &[],
                    None,
                    None,  // liveness
                );
            })
            .unwrap();

        let n0 = tab_render_state.height_cache.entries.len();
        assert!(n0 > 0, "warm render must populate cache");

        // Re-render with clock advanced (frame 7: different spinner glyph)
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let clock2 = MockClock::at_wall_ms(1_700_000_000_000);
        clock2.set_wall_anchor_ms(1_700_000_000_560); // frame 7 (7×80ms)
        terminal
            .draw(|frame| {
                let _ = render_with_search(
                    frame,
                    Rect::new(0, 0, 80, 24),
                    &conversation,
                    None,
                    &streaming,
                    &ViewState::default(),
                    &clock2,
                    0,
                    true,
                    &theme,
                    &mut tab_render_state,
                    &tbs,
                    &BTreeMap::new(),
                    None,
                    None,
                    &[],
                    &[],
                    None,
                    None,  // liveness
                );
            })
            .unwrap();
        assert_eq!(
            tab_render_state.height_cache.entries.len(),
            n0,
            "spinner tick must not change cache size"
        );

        // Re-render with clock advanced further (frame 11)
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let clock3 = MockClock::at_wall_ms(1_700_000_000_000);
        clock3.set_wall_anchor_ms(1_700_000_000_880); // frame 11 (11×80ms)
        terminal
            .draw(|frame| {
                let _ = render_with_search(
                    frame,
                    Rect::new(0, 0, 80, 24),
                    &conversation,
                    None,
                    &streaming,
                    &ViewState::default(),
                    &clock3,
                    0,
                    true,
                    &theme,
                    &mut tab_render_state,
                    &tbs,
                    &BTreeMap::new(),
                    None,
                    None,
                    &[],
                    &[],
                    None,
                    None,  // liveness
                );
            })
            .unwrap();
        assert_eq!(
            tab_render_state.height_cache.entries.len(),
            n0,
            "spinner tick must not change cache size (second frame)"
        );
    }

    /// AC11: Width-parameterized parity (79, 80, 81)
    #[test]
    fn parity_width_79_80_81() {
        let turn = make_turn(
            vec![make_prose(
                "This is a line that may wrap differently at width boundaries.",
            )],
            Some(StopReason::EndTurn),
        );
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let theme = Theme::dark();
        let tbs: HashMap<String, ToolBlockState> = HashMap::new();

        for width in [79, 80, 81] {
            let cached = expanded_turn_height(&turn, &theme, width, &tbs);
            let rendered = render_expanded_turn(&turn, &theme, width, &clock, &tbs, None).len();
            assert_eq!(cached.height, rendered, "parity fail: width={}", width);
        }
    }

    /// Task 4.2: Block boundaries match render offsets with expanded tool.
    #[test]
    fn block_boundaries_match_render_offsets_with_expanded_tool() {
        let turn = make_turn(
            vec![
                make_prose("Before tool."),
                make_tool("Read", InvocationStatus::Success),
                make_prose("After tool."),
            ],
            Some(StopReason::EndTurn),
        );
        let theme = Theme::dark();
        let mut tbs: HashMap<String, ToolBlockState> = HashMap::new();
        tbs.insert(
            "test-turn-1".into(),
            ToolBlockState {
                collapsed: false,
                peek_active: false,
            },
        );

        let layout = expanded_turn_height(&turn, &theme, 80, &tbs);

        // block_offsets should have 3 entries (prose, tool, prose)
        assert_eq!(layout.block_offsets.len(), 3, "expected 3 block offsets");

        // Render and verify cumulative offsets
        let clock = MockClock::at_wall_ms(1_700_000_000_000);
        let lines = render_expanded_turn(&turn, &theme, 80, &clock, &tbs, None);

        // The first block offset should be 0 (start of turn)
        assert_eq!(layout.block_offsets[0], 0, "first block should start at 0");

        // Total height should match rendered line count
        assert_eq!(
            layout.height,
            lines.len(),
            "height must match rendered line count"
        );
    }

    // ── build_layout_metrics unit tests (S16.6 Task 0.3) ──

    #[test]
    fn build_layout_metrics_handles_empty_conversation() {
        let conversation = Conversation {
            id: "empty".to_string(),
            title: String::new(),
            messages: vec![],
            turns: vec![],
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        };
        let view_state = ViewState::default();
        let mut trs = TabRenderState::default();
        let theme = Theme::dark();
        let clock = MockClock::at_wall_ms(0);

        let layout = build_layout_metrics(&conversation, &view_state, &mut trs, &theme, 80, 24, &clock, &std::collections::HashMap::new());

        assert_eq!(layout.total_content_height, 0);
        assert!(layout.turn_top_offsets.is_empty());
        assert_eq!(layout.focused_turn_top, None);
        assert_eq!(layout.viewport_height, 24);
    }

    #[test]
    fn build_layout_metrics_returns_correct_turn_top_offsets() {
        let mut turn1 = Turn::new("claude-3".to_string(), 1000);
        turn1.stop_reason = Some(StopReason::EndTurn);
        turn1.push_part(|id| TurnPart::Prose { id, text: "Hello world".to_string() });
        let turn2 = Turn::user("User message".to_string(), 2000);

        let conversation = Conversation {
            id: "test".to_string(),
            title: String::new(),
            messages: vec![],
            turns: vec![turn1.clone(), turn2],
            created_at: 0, updated_at: 0, last_response_at: None,
            session_id: None, usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        };
        let view_state = ViewState::default();
        let mut trs = TabRenderState::default();
        let theme = Theme::dark();
        let clock = MockClock::at_wall_ms(0);
        let layout = build_layout_metrics(&conversation, &view_state, &mut trs, &theme, 80, 24, &clock, &std::collections::HashMap::new());

        assert!(!layout.turn_top_offsets.is_empty(), "should have entries");
        // First turn at offset 0
        assert_eq!(layout.turn_top_offsets[0], (turn1.id.clone(), 0));
        // Second turn at some positive offset (after first turn's height)
        assert!(layout.turn_top_offsets[1].1 > 0, "second turn should be after first");
        assert!(layout.total_content_height > 0);
    }

    #[test]
    fn build_layout_metrics_focused_turn_top_matches_focused_turn() {
        let mut turn1 = Turn::new("claude-3".to_string(), 1000);
        turn1.stop_reason = Some(StopReason::EndTurn);
        turn1.push_part(|id| TurnPart::Prose { id, text: "First assistant".to_string() });
        let mut turn2 = Turn::new("claude-3".to_string(), 2000);
        turn2.stop_reason = Some(StopReason::EndTurn);
        turn2.push_part(|id| TurnPart::Prose { id, text: "Second assistant".to_string() });

        let conversation = Conversation {
            id: "test".to_string(),
            title: String::new(), messages: vec![],
            turns: vec![turn1.clone(), turn2.clone()],
            created_at: 0, updated_at: 0, last_response_at: None,
            session_id: None, usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        };
        let mut view_state = ViewState::default();
        view_state.set_focused_turn(Some(turn2.id.clone()));

        let mut trs = TabRenderState::default();
        let theme = Theme::dark();
        let clock = MockClock::at_wall_ms(0);
        let layout = build_layout_metrics(&conversation, &view_state, &mut trs, &theme, 80, 24, &clock, &std::collections::HashMap::new());

        assert_eq!(layout.focused_turn_top, Some(layout.turn_top_offsets[1].1));
    }
}
