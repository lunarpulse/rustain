pub mod virtual_scroll;
pub mod word_wrap;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use std::collections::{BTreeMap, HashMap};

use crate::adapters::tui::state::HeightCache;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::feedback_block;
use crate::adapters::tui::widgets::tool_block::{self, ToolBlockState};
use crate::domain::models::{
    ContentBlockType, Conversation, FeedbackBlock, MessageRole, StopReason, StreamingState,
};
use crate::domain::services::search::SearchMatch;

use super::empty_state;
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
    };
    let role_color = match msg.role {
        MessageRole::User => theme.colors.accent,
        MessageRole::Assistant => theme.colors.fg_secondary,
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

/// Render the chat pane with virtual scrolling (viewport culling).
///
/// Returns `RenderResult` with total content height and boundary data.
///
/// Backward-compatible wrapper around `render_with_search` for callers that
/// don't need search highlighting or bookmarks (pre-4-4 tests, non-search
/// code paths). The main binary calls `render_with_search` directly; this
/// wrapper is only exercised by integration tests in `tests/`, so it looks
/// dead from a lib-only compile.
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
        streaming,
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
    streaming: &StreamingState,
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
) -> RenderResult {
    let empty = RenderResult {
        total_content_height: 0,
        block_boundaries: Vec::new(),
        message_boundaries: Vec::new(),
        user_message_boundaries: Vec::new(),
        focused_tool_id: None,
    };

    // Empty state: no messages and not streaming
    if conversation.messages.is_empty() && !streaming.is_streaming {
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

    // Phase 1: Compute per-message heights (using cache where possible)
    // and build boundary data.
    let mut message_heights: Vec<usize> = Vec::with_capacity(msg_count + 1);
    let mut block_boundaries: Vec<usize> = Vec::new();
    let mut message_boundaries: Vec<usize> = Vec::new();
    let mut user_message_boundaries: Vec<usize> = Vec::new();
    let mut cumulative_offset: usize = 0;

    for (i, msg) in conversation.messages.iter().enumerate() {
        if i > 0 {
            cumulative_offset += spacing;
        }

        // Track boundaries. `message_boundaries` records **every** message so
        // the status-bar `msg N/M` counter and rewind/fork target resolution
        // map 1:1 to `conversation.messages` indices. `user_message_boundaries`
        // records user turns only, for `{`/`}` jump-between-turns navigation.
        message_boundaries.push(cumulative_offset);
        if msg.role == MessageRole::User {
            user_message_boundaries.push(cumulative_offset);
        }
        block_boundaries.push(cumulative_offset);

        // Get or compute height (invalidate cache if tool block states changed)
        let has_error = msg.content_blocks.contains(&ContentBlockType::Error);
        let is_cancelled = msg.stop_reason == Some(StopReason::Cancelled);
        let is_bookmarked = bookmarks.binary_search(&i).is_ok();
        let mut h =
            compute_message_height(&msg.content, has_error, is_cancelled, is_bookmarked, width);

        // Add tool block heights
        for tc in &msg.tool_calls {
            let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
            h += tool_block::tool_block_height(tc, &tb_state);
            block_boundaries.push(cumulative_offset + h);
        }

        // Use height cache keyed by message ID. If the cached value diverges from the freshly
        // computed value (e.g., a tool result with an error arrived and changed the height),
        // invalidate the whole cache so subsequent messages stay in sync (AC2, DF-061).
        if let Some(cached) = height_cache.get(&msg.id) {
            if cached == h {
                h = cached; // Cache hit — values agree, no divergence.
            } else {
                // Stale entry detected: height changed without an explicit invalidation.
                // Invalidate all to keep block_boundaries coherent for downstream messages.
                height_cache.invalidate_all();
                height_cache.set(msg.id.clone(), h);
            }
        } else {
            height_cache.set(msg.id.clone(), h);
        }

        message_heights.push(h);
        cumulative_offset += h;
    }

    // Streaming content height
    let streaming_height = if streaming.is_streaming {
        if !conversation.messages.is_empty() {
            cumulative_offset += spacing;
        }
        block_boundaries.push(cumulative_offset);

        let mut h = if streaming.current_text_buffer.is_empty() {
            if streaming.current_blocks.contains(&ContentBlockType::Error) {
                2 // "Assistant:" + error line
            } else {
                1 // typing indicator
            }
        } else {
            let has_error = streaming.current_blocks.contains(&ContentBlockType::Error);
            1 + if has_error {
                wrap_text(&streaming.current_text_buffer, width).len()
            } else {
                markdown::compute_height(
                    &streaming.current_text_buffer,
                    width,
                    &markdown::RenderOptions::default(),
                )
            }
        };

        // Add heights for streaming tool calls
        for tc in streaming.active_tool_calls.values() {
            let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
            h += tool_block::tool_block_height(tc, &tb_state);
            block_boundaries.push(cumulative_offset + h);
        }

        cumulative_offset += h;
        h
    } else {
        0
    };

    // Pre-compute feedback block contribution so visible_start/visible_end include them.
    // Without this, auto_scroll viewport is computed before feedback heights are known,
    // causing feedback blocks to be silently dropped from the render (AC1, DF-079).
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

    // Phase 2: Determine visible range using offset-from-bottom model
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

    // Phase 3: Only build Line objects for visible messages (viewport culling)
    let mut lines: Vec<Line> = Vec::new();
    let mut line_offset: usize = 0;

    for (i, msg) in conversation.messages.iter().enumerate() {
        if i > 0 {
            // Spacing
            let spacing_end = line_offset + spacing;
            if spacing_end > visible_start && line_offset < visible_end {
                // Some spacing lines are visible
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
            // This message is (at least partially) visible — render it
            // Fork point marker: last message of a forked conversation (AC3, Story 4-3a).
            // Only the forked conversation gets the marker (not the original).
            let is_fork_point = conversation.fork_source.is_some()
                && i == conversation.messages.len().saturating_sub(1);
            // If the focused search match belongs to this message, compute
            // its local ordinal (0-based position among matches in this
            // message) so `render_message` knows which match in this message
            // should use the focused style.
            //
            // Second-audit Fix 2: use the full `search_matches` list — not
            // just the focused pointer — to compute the true ordinal. Matches
            // are sorted by `(message_index, byte_start)`, so filtering by
            // `message_index == i` yields the in-message matches in order,
            // and `position()` gives the focused ordinal within that subset.
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
            let msg_lines = render_message(
                msg,
                width,
                theme,
                is_fork_point,
                is_bookmarked,
                search_query,
                focused_local_ordinal,
            );
            for (j, line) in msg_lines.into_iter().enumerate() {
                let abs_line = line_offset + j;
                if abs_line >= visible_start && abs_line < visible_end {
                    lines.push(line);
                }
            }

            // Render tool blocks for this message
            let text_height = compute_message_height(
                &msg.content,
                msg.content_blocks.contains(&ContentBlockType::Error),
                msg.stop_reason == Some(StopReason::Cancelled),
                is_bookmarked,
                width,
            );
            let mut tool_line_offset = line_offset + text_height;
            for tc in &msg.tool_calls {
                let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                let tb_lines =
                    tool_block::render_tool_block_lines(tc, theme, &tb_state, area.width);
                for (j, line) in tb_lines.into_iter().enumerate() {
                    let abs_line = tool_line_offset + j;
                    if abs_line >= visible_start && abs_line < visible_end {
                        lines.push(line);
                    }
                }
                tool_line_offset += tool_block::tool_block_height(tc, &tb_state);
            }
        }
        // else: skip this message entirely (viewport culling)

        line_offset += msg_height;
    }

    // Streaming content
    if streaming.is_streaming {
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

        let stream_end = line_offset + streaming_height;
        if stream_end > visible_start && line_offset < visible_end {
            let stream_lines = render_streaming(streaming, width, theme);
            let text_line_count = stream_lines.len();
            for (j, line) in stream_lines.into_iter().enumerate() {
                let abs_line = line_offset + j;
                if abs_line >= visible_start && abs_line < visible_end {
                    lines.push(line);
                }
            }

            // Render streaming tool calls
            let mut tool_line_offset = line_offset + text_line_count;
            for tc in streaming.active_tool_calls.values() {
                let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
                let tb_lines =
                    tool_block::render_tool_block_lines(tc, theme, &tb_state, area.width);
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

    // Render feedback blocks at the bottom of conversation
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
            block_boundaries.push(line_offset);
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

    // Recalculate total height with feedback blocks
    total_content_height = cumulative_offset;

    // AC3: Jump-to-bottom indicator when scrolled away from bottom during streaming
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
        // Replace last visible line with indicator
        let last = lines.len() - 1;
        lines[last] = indicator_line;
    }

    let widget = Paragraph::new(Text::from(lines));
    frame.render_widget(widget, area);

    // Determine focused tool block: find the tool block closest to the top of the viewport.
    // Scan messages for tool calls whose block boundary falls within the visible range.
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

/// Render streaming content into Line objects.
fn render_streaming<'a>(streaming: &StreamingState, width: usize, theme: &Theme) -> Vec<Line<'a>> {
    let mut lines = Vec::new();

    if streaming.current_text_buffer.is_empty() {
        let has_streaming_error = streaming.current_blocks.contains(&ContentBlockType::Error);
        if has_streaming_error {
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

        let has_streaming_error = streaming.current_blocks.contains(&ContentBlockType::Error);
        if has_streaming_error {
            let content_lines = wrap_text(&streaming.current_text_buffer, width);
            for text in content_lines {
                lines.push(Line::from(Span::styled(
                    text,
                    Style::default().fg(theme.colors.error),
                )));
            }
        } else {
            let parsed_lines = markdown::render(
                &streaming.current_text_buffer,
                width,
                theme,
                &markdown::RenderOptions::default(),
            );
            lines.extend(parsed_lines);
        }
    }

    lines
}
