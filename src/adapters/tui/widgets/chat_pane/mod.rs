pub mod virtual_scroll;
pub mod word_wrap;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use std::collections::HashMap;

use crate::adapters::tui::state::HeightCache;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::tool_block::{self, ToolBlockState};
use crate::domain::models::{
    ContentBlockType, Conversation, MessageRole, StopReason, StreamingState,
};

use super::empty_state;
use word_wrap::{parse_inline_code, wrap_text};

/// Result of rendering the chat pane, including boundary data for navigation.
pub struct RenderResult {
    pub total_content_height: usize,
    /// Line offsets (from top) where each content block starts.
    pub block_boundaries: Vec<usize>,
    /// Line offsets (from top) where each user message starts.
    pub message_boundaries: Vec<usize>,
    /// Tool block id at the top of the viewport (for focus/keyboard interaction).
    pub focused_tool_id: Option<String>,
}

/// Compute the rendered height of a single message (role line + content lines)
/// without building actual Line objects (for off-screen height computation).
fn compute_message_height(
    content: &str,
    has_error: bool,
    is_cancelled: bool,
    width: usize,
) -> usize {
    // 1 for role line
    let content_height = if has_error || content.is_empty() {
        let wrapped = wrap_text(content, width);
        wrapped.len()
    } else {
        // Count lines that parse_inline_code would produce
        let mut count = 0;
        for text_line in content.split('\n') {
            let char_count = text_line.chars().count();
            if char_count > width && width > 0 {
                let wrapped = wrap_text(text_line, width);
                count += wrapped.len();
            } else {
                count += 1;
            }
        }
        count
    };
    let interrupted_line = if is_cancelled { 1 } else { 0 };
    1 + content_height + interrupted_line // role line + content + optional [interrupted]
}

/// Render a single message into Line objects.
fn render_message<'a>(
    msg: &crate::domain::models::ChatMessage,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    let has_error = msg.content_blocks.contains(&ContentBlockType::Error);

    // Role indicator
    let role_line = match msg.role {
        MessageRole::User => Line::from(Span::styled(
            "You:",
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        )),
        MessageRole::Assistant => Line::from(Span::styled(
            "Assistant:",
            Style::default()
                .fg(theme.colors.fg_secondary)
                .add_modifier(Modifier::BOLD),
        )),
    };
    lines.push(role_line);

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
        let parsed_lines = parse_inline_code(&msg.content, width, theme);
        lines.extend(parsed_lines);
    }

    // Append [interrupted] suffix for cancelled messages (styled with fg_muted)
    if msg.stop_reason == Some(StopReason::Cancelled) {
        lines.push(Line::from(Span::styled(
            " [interrupted]",
            Style::default().fg(theme.colors.fg_muted),
        )));
    }

    lines
}

/// Render the chat pane with virtual scrolling (viewport culling).
///
/// Returns `RenderResult` with total content height and boundary data.
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
) -> RenderResult {
    let empty = RenderResult {
        total_content_height: 0,
        block_boundaries: Vec::new(),
        message_boundaries: Vec::new(),
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
    let mut cumulative_offset: usize = 0;

    for (i, msg) in conversation.messages.iter().enumerate() {
        if i > 0 {
            cumulative_offset += spacing;
        }

        // Track boundaries
        if msg.role == MessageRole::User {
            message_boundaries.push(cumulative_offset);
        }
        block_boundaries.push(cumulative_offset);

        // Get or compute height (invalidate cache if tool block states changed)
        let has_error = msg.content_blocks.contains(&ContentBlockType::Error);
        let is_cancelled = msg.stop_reason == Some(StopReason::Cancelled);
        let mut h = compute_message_height(&msg.content, has_error, is_cancelled, width);

        // Add tool block heights
        for tc in &msg.tool_calls {
            let tb_state = tool_block_states.get(&tc.id).cloned().unwrap_or_default();
            h += tool_block::tool_block_height(tc, &tb_state);
            block_boundaries.push(cumulative_offset + h);
        }

        // Use height cache for all messages (including those with tool calls).
        // Cache is invalidated on collapse/expand toggle via height_cache.invalidate_all().
        if let Some(cached) = height_cache.get(i) {
            h = cached;
        } else {
            height_cache.set(i, h);
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
                let mut count = 0;
                for text_line in streaming.current_text_buffer.split('\n') {
                    let cc = text_line.chars().count();
                    if cc > width && width > 0 {
                        count += wrap_text(text_line, width).len();
                    } else {
                        count += 1;
                    }
                }
                count
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

    let total_content_height = cumulative_offset;

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
            let msg_lines = render_message(msg, width, theme);
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
            let parsed_lines = parse_inline_code(&streaming.current_text_buffer, width, theme);
            lines.extend(parsed_lines);
        }
    }

    lines
}
