//! PermissionCard widget — renders inline permission prompts in the chat stream.
//!
//! Story 19.20c: the card shows the exact pending input (the shared `A2`
//! presentation) in the host-delimited body region, first ≤ 4 wrapped rows,
//! then TM-09's T5 marker; `[r] Review` opens the complete input. Standalone
//! and the daemon-attached rich client paint through these same functions.

use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::delimited_body::{approval_body_rows, wrap_text};
use crate::adapters::tui::widgets::tool_block::display_tool_name;
use crate::domain::models::ToolRisk;
use crate::domain::models::tool_call::ApprovalSource;
use crate::domain::services::approval_presentation::{
    BODY_DELIMITER, CARD_BODY_ROWS, MORE_MARKER, body_lines,
};
use crate::domain::services::peer_text::sanitize_peer_text_line;

const CARD_BORDER: &str = "┃ ";

/// What a deciding surface holds of a request's exact input.
#[derive(Clone, Copy, Debug)]
pub enum ExactInput<'a> {
    /// The complete exact pending value.
    Ready(&'a serde_json::Value),
    /// Attached writer only: the exact input is still arriving in parts;
    /// `summary` is the daemon's one-line per-tool summary.
    Receiving { summary: &'a str },
}

/// One pending tool approval as a card or Review renders it.
#[derive(Clone, Copy, Debug)]
pub struct PermissionCard<'a> {
    pub source: &'a ApprovalSource,
    pub tool_name: &'a str,
    pub risk: ToolRisk,
    pub input: ExactInput<'a>,
    /// Requests queued behind this one.
    pub queue_len: usize,
}

/// The pinned Review reader: `header` and `footer` stay put while `body`
/// scrolls (PageUp/PageDown).
pub struct PermissionReview {
    pub header: Vec<Line<'static>>,
    pub body: Vec<Line<'static>>,
    pub footer: Vec<Line<'static>>,
}

/// Fixed metadata must never starve the scrollable exact input or decision keys.
/// Both terminal surfaces use the same allocation; metadata may be elided, input may not.
pub(crate) fn permission_review_rows(
    height: u16,
    header_len: usize,
    footer_len: usize,
) -> (usize, usize, usize) {
    let height = usize::from(height);
    let footer = footer_len.min(height.saturating_sub(1));
    let header = header_len.min(height.saturating_sub(footer + 1));
    (header, height.saturating_sub(header + footer), footer)
}

/// Ellipses apply only to identity/source metadata, never to exact input.
fn bounded_metadata(text: &str, width: usize, limit: usize) -> Vec<String> {
    let (mut rows, more) = wrap_text(text, width.saturating_sub(1).max(1), limit);
    if more && let Some(last) = rows.last_mut() {
        last.push('…');
    }
    rows
}

/// Story 19.20c: presentation-only state, pinned to the approval RequestId.
#[derive(Default)]
pub struct PermissionReaderState {
    pub review: Option<crate::domain::models::RequestId>,
    pub scroll: usize,
    pub scroll_max: usize,
    pub viewport_rows: usize,
    cached: Option<PreparedPermission>,
}

struct PreparedPermission {
    id: crate::domain::models::RequestId,
    width: u16,
    queue_len: usize,
    card: Vec<Line<'static>>,
    review: Option<PermissionReview>,
}

impl PermissionReaderState {
    pub fn open(&mut self, id: crate::domain::models::RequestId) {
        self.review = Some(id);
        self.scroll = 0;
    }

    pub fn back(&mut self) {
        self.review = None;
        self.scroll = 0;
    }

    pub fn page(&mut self, down: bool) {
        let step = self.viewport_rows.saturating_sub(1).max(1);
        self.scroll = if down {
            self.scroll.saturating_add(step).min(self.scroll_max)
        } else {
            self.scroll.saturating_sub(step)
        };
    }
}

pub(crate) fn paint_rows(frame: &mut Frame<'_>, area: Rect, rows: &[Line<'static>], scroll: usize) {
    for (offset, line) in rows
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .enumerate()
    {
        frame.render_widget(
            line,
            Rect::new(area.x, area.y + offset as u16, area.width, 1),
        );
    }
}

/// Render the standalone permission owner, including deny-feedback. The same
/// chat-pane viewport is used by the event loop and production-path tests.
pub fn render_permission_surface(
    frame: &mut Frame<'_>,
    area: Rect,
    pending: Option<&crate::adapters::tui::state::PendingPermission>,
    feedback: Option<&crate::adapters::tui::state::FeedbackInputState>,
    queue_len: usize,
    reader: &mut PermissionReaderState,
    theme: &Theme,
) {
    if let Some(feedback) = feedback {
        let rows = render_feedback_input_lines(&feedback.buffer, theme);
        let height = (rows.len() as u16).min(area.height);
        let prompt = Rect::new(
            area.x,
            area.bottom().saturating_sub(height),
            area.width,
            height,
        );
        frame.render_widget(ratatui::widgets::Clear, prompt);
        frame.render_widget(ratatui::widgets::Paragraph::new(rows), prompt);
    } else if let Some(pending) = pending {
        render_permission_overlay(frame, area, pending, queue_len, reader, theme);
    }
}

/// Paint the actual standalone deciding surface. Layout is cached until the
/// request or width changes; rendering never reads the approval runtime.
pub fn render_permission_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    pending: &crate::adapters::tui::state::PendingPermission,
    queue_len: usize,
    reader: &mut PermissionReaderState,
    theme: &Theme,
) {
    let card = PermissionCard {
        source: &pending.source,
        tool_name: &pending.tool_name,
        risk: pending.risk,
        input: ExactInput::Ready(&pending.input),
        queue_len,
    };
    if reader
        .cached
        .as_ref()
        .is_none_or(|cached| cached.id != pending.id || cached.width != area.width)
    {
        reader.cached = Some(PreparedPermission {
            id: pending.id.clone(),
            width: area.width,
            queue_len,
            card: render_permission_card_lines(&card, area.width, theme),
            review: None,
        });
    }
    let cached = reader.cached.as_mut().expect("prepared above");
    if cached.queue_len != queue_len {
        cached.card = render_permission_card_lines(&card, area.width, theme);
        cached.queue_len = queue_len;
    }
    if reader.review.as_ref() == Some(&pending.id) {
        let review = cached
            .review
            .get_or_insert_with(|| render_permission_review(&card, area.width, theme));
        frame.render_widget(ratatui::widgets::Clear, area);
        let (header_rows, body_rows, footer_rows) =
            permission_review_rows(area.height, review.header.len(), review.footer.len());
        reader.viewport_rows = body_rows;
        reader.scroll_max = review.body.len().saturating_sub(body_rows);
        reader.scroll = reader.scroll.min(reader.scroll_max);
        paint_rows(
            frame,
            Rect::new(area.x, area.y, area.width, header_rows as u16),
            &review.header,
            0,
        );
        paint_rows(
            frame,
            Rect::new(
                area.x,
                area.y + header_rows as u16,
                area.width,
                body_rows as u16,
            ),
            &review.body,
            reader.scroll,
        );
        paint_rows(
            frame,
            Rect::new(
                area.x,
                area.bottom().saturating_sub(footer_rows as u16),
                area.width,
                footer_rows as u16,
            ),
            &review.footer,
            0,
        );
    } else {
        let height = cached.card.len().min(area.height as usize);
        let prompt_area = Rect::new(
            area.x,
            area.bottom().saturating_sub(height as u16),
            area.width,
            height as u16,
        );
        let old_max = reader.scroll_max;
        reader.viewport_rows = height;
        reader.scroll_max = cached.card.len().saturating_sub(height);
        // At the minimum viewport keep the decision keys visible initially;
        // PageUp reaches the header/body instead of clipping the controls.
        if reader.scroll_max != old_max {
            reader.scroll = reader.scroll_max;
        }
        reader.scroll = reader.scroll.min(reader.scroll_max);
        frame.render_widget(ratatui::widgets::Clear, prompt_area);
        paint_rows(frame, prompt_area, &cached.card, reader.scroll);
    }
}

fn source_prefix(source: &ApprovalSource) -> String {
    match source {
        ApprovalSource::ForegroundSubagent { subagent_type, .. } => {
            format!("[subagent: {subagent_type}] ")
        }
        ApprovalSource::BackgroundAgent { subagent_type, .. } => {
            format!("[background: {subagent_type}] ")
        }
        ApprovalSource::RemotePeer { peer_id, .. } => format!("[remote peer] {peer_id} "),
        _ => String::new(),
    }
}

fn tool_label(tool_name: &str) -> String {
    sanitize_peer_text_line(&display_tool_name(tool_name)).into_owned()
}

fn risk_label(risk: ToolRisk) -> &'static str {
    match risk {
        ToolRisk::Safe => "safe",
        ToolRisk::Standard => "standard",
        ToolRisk::Elevated => "elevated",
        ToolRisk::Blocked => "blocked",
    }
}

/// `Always` label: MCP keeps `Always for [server]`.
fn always_label(tool_name: &str) -> String {
    tool_name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .map_or_else(
            || "Always".to_owned(),
            |(server, _)| {
                let server = sanitize_peer_text_line(server);
                let server = bounded_metadata(&server, 17, 1).pop().unwrap_or_default();
                format!("Always for [{server}]")
            },
        )
}

/// Pack `(key, label)` items into rows of at most `width` cells; an item
/// never splits across rows unless it alone is wider than a row.
fn key_rows(
    items: &[(&str, String, Color)],
    width: usize,
    prefix: &'static str,
    border: Style,
    label: Style,
) -> Vec<Line<'static>> {
    let width = width.saturating_sub(prefix.chars().count()).max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cells = usize::MAX;
    for (key, text, color) in items {
        let item_cells = key.chars().count() + 1 + text.chars().count();
        let gap = if cells == 0 || cells == usize::MAX {
            0
        } else {
            2
        };
        if cells == usize::MAX || cells + gap + item_cells > width {
            if item_cells > width {
                let joined = format!("{key} {text}");
                for part in wrap_text(&joined, width, usize::MAX).0 {
                    rows.push(vec![Span::styled(part, label)]);
                }
                cells = usize::MAX;
                continue;
            }
            rows.push(Vec::new());
            cells = 0;
        }
        let row = rows.last_mut().expect("row pushed above");
        if cells > 0 {
            row.push(Span::styled("  ", label));
            cells += 2;
        }
        row.push(Span::styled((*key).to_owned(), Style::default().fg(*color)));
        row.push(Span::styled(format!(" {text}"), label));
        cells += item_cells;
    }
    rows.into_iter()
        .map(|spans| {
            let mut line = vec![Span::styled(prefix, border)];
            line.extend(spans);
            Line::from(line)
        })
        .collect()
}

fn delimited_rows(
    tool_name: &str,
    input: &serde_json::Value,
    width: usize,
    limit: usize,
    border: Option<Style>,
    theme: &Theme,
) -> (Vec<Line<'static>>, bool) {
    let lines = body_lines(tool_name, input);
    let border_cells = if border.is_some() { 2 } else { 0 };
    let text_width = width.saturating_sub(border_cells + BODY_DELIMITER.chars().count());
    let (rows, more) = approval_body_rows(&lines, text_width, limit);
    let delimiter = Style::default().fg(theme.colors.accent);
    let label = Style::default()
        .fg(theme.colors.fg_secondary)
        .add_modifier(Modifier::BOLD);
    let value = Style::default().fg(theme.colors.fg_primary);
    let rendered = rows
        .into_iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(4);
            if let Some(border) = border {
                spans.push(Span::styled(CARD_BORDER, border));
            }
            spans.push(Span::styled(BODY_DELIMITER, delimiter));
            let (head, tail) = row.text.split_at(row.label_len);
            if !head.is_empty() {
                spans.push(Span::styled(head.to_owned(), label));
            }
            spans.push(Span::styled(tail.to_owned(), value));
            Line::from(spans)
        })
        .collect();
    (rendered, more)
}

fn receiving_rows(
    summary: &str,
    width: usize,
    border: Option<Style>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let border_cells = if border.is_some() { 2 } else { 0 };
    let text_width = width
        .saturating_sub(border_cells + BODY_DELIMITER.chars().count())
        .max(1);
    let delimiter = Style::default().fg(theme.colors.accent);
    let value = Style::default().fg(theme.colors.fg_primary);
    let summary = sanitize_peer_text_line(summary);
    // The summary is one line; while the parts arrive it gets at most the
    // card's body rows, ending in `…` when cut (the complete input follows).
    let (mut rows, more) = wrap_text(&summary, text_width, CARD_BODY_ROWS);
    if more && let Some(last) = rows.last_mut() {
        last.pop();
        last.push('…');
    }
    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(3);
            if let Some(border) = border {
                spans.push(Span::styled(CARD_BORDER, border));
            }
            spans.push(Span::styled(BODY_DELIMITER, delimiter));
            spans.push(Span::styled(row, value));
            Line::from(spans)
        })
        .collect();
    let mut notice = Vec::with_capacity(2);
    if let Some(border) = border {
        notice.push(Span::styled(CARD_BORDER, border));
    }
    notice.push(Span::styled(
        "receiving exact input…",
        Style::default().fg(theme.colors.fg_secondary),
    ));
    lines.push(Line::from(notice));
    lines
}

/// Story 19.20c `A2`/`A5` — the permission card at `width` cells: header
/// (`┃ [source prefix] <Tool>` + queue indicator), the first ≤ 4 delimited
/// body rows of the exact input, T5's marker whenever more exists, and the
/// decision keys with `[r] Review` always offered. Exact input is never
/// truncated in Review; long header/source metadata is marked with an ellipsis.
pub fn render_permission_card_lines(
    card: &PermissionCard<'_>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = usize::from(width).max(8);
    let border = Style::default()
        .fg(theme.colors.permission_border)
        .add_modifier(Modifier::BOLD);
    let secondary = Style::default().fg(theme.colors.fg_secondary);
    let mut lines = Vec::new();

    let prefix = source_prefix(card.source);
    let tool = tool_label(card.tool_name);
    let queue = if card.queue_len > 0 {
        format!("  [{} more queued]", card.queue_len)
    } else {
        String::new()
    };
    let header = sanitize_peer_text_line(&format!("{prefix}{tool}{queue}")).into_owned();
    let prefix_len = sanitize_peer_text_line(&prefix).len();
    for (index, row) in bounded_metadata(&header, width - 2, 2)
        .into_iter()
        .enumerate()
    {
        let mut spans = vec![Span::styled(CARD_BORDER, border)];
        if index == 0 && prefix_len > 0 && prefix_len <= row.len() {
            let (head, tail) = row.split_at(prefix_len);
            spans.push(Span::styled(
                head.to_owned(),
                Style::default().fg(theme.colors.subagent_attribution),
            ));
            spans.push(Span::styled(
                tail.to_owned(),
                Style::default()
                    .fg(theme.colors.tool_name)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                row,
                Style::default()
                    .fg(theme.colors.tool_name)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
    }

    match card.input {
        ExactInput::Ready(input) => {
            let (body, more) = delimited_rows(
                card.tool_name,
                input,
                width,
                CARD_BODY_ROWS,
                Some(border),
                theme,
            );
            lines.extend(body);
            if more {
                for row in wrap_text(MORE_MARKER, width - 2, usize::MAX).0 {
                    lines.push(Line::from(vec![
                        Span::styled(CARD_BORDER, border),
                        Span::styled(row, secondary),
                    ]));
                }
            }
        }
        ExactInput::Receiving { summary } => {
            lines.extend(receiving_rows(summary, width, Some(border), theme));
        }
    }

    lines.extend(key_rows(
        &[
            ("[y]", "Allow".to_owned(), theme.colors.success),
            ("[s]", "Session".to_owned(), theme.colors.accent),
            ("[a]", always_label(card.tool_name), theme.colors.warning),
        ],
        width,
        CARD_BORDER,
        border,
        secondary,
    ));
    lines.extend(key_rows(
        &[
            ("[n]", "Deny".to_owned(), theme.colors.error),
            (
                "[f]",
                "Deny + feedback".to_owned(),
                theme.colors.auto_sent_border,
            ),
            ("[r]", "Review".to_owned(), theme.colors.accent),
        ],
        width,
        CARD_BORDER,
        border,
        secondary,
    ));
    lines
}

/// Story 19.20c `A4`/`A5` — the pinned Review reader at `width` cells:
/// heading `Exact tool input (not run yet) · <Tool>`, the source/risk row,
/// every delimited body row (wrapped, nothing elided), and the footer keys.
pub fn render_permission_review(
    card: &PermissionCard<'_>,
    width: u16,
    theme: &Theme,
) -> PermissionReview {
    let width = usize::from(width).max(8);
    let primary = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::BOLD);
    let secondary = Style::default().fg(theme.colors.fg_secondary);
    let tool = tool_label(card.tool_name);
    let mut header: Vec<Line<'static>> = bounded_metadata(
        &format!("Exact tool input (not run yet) · {tool}"),
        width,
        2,
    )
    .into_iter()
    .map(|row| Line::from(Span::styled(row, primary)))
    .collect();
    let source_row = sanitize_peer_text_line(&format!(
        "risk: {} {}",
        risk_label(card.risk),
        source_prefix(card.source)
    ))
    .into_owned();
    header.extend(
        bounded_metadata(&source_row, width, 1)
            .into_iter()
            .map(|row| Line::from(Span::styled(row, secondary))),
    );
    let body = match card.input {
        ExactInput::Ready(input) => {
            delimited_rows(card.tool_name, input, width, usize::MAX, None, theme).0
        }
        ExactInput::Receiving { summary } => receiving_rows(summary, width, None, theme),
    };
    let footer = key_rows(
        &[
            ("[y]", "Allow".to_owned(), theme.colors.success),
            ("[n]", "Deny".to_owned(), theme.colors.error),
            (
                "[f]",
                "Deny + feedback".to_owned(),
                theme.colors.auto_sent_border,
            ),
            ("[Esc]", "Back".to_owned(), theme.colors.fg_secondary),
            ("PgUp/PgDn", "scroll".to_owned(), theme.colors.fg_secondary),
        ],
        width,
        "",
        secondary,
        secondary,
    );
    PermissionReview {
        header,
        body,
        footer,
    }
}

/// Render feedback input lines for the deny-with-feedback mini-input (AC5).
///
/// Visual spec:
/// ```text
/// ┃ Feedback: <text>▋                                    ┃
/// ┃ [Enter] send  [Esc] cancel                           ┃
/// ```
pub fn render_feedback_input_lines<'a>(buffer: &str, theme: &'a Theme) -> Vec<Line<'a>> {
    let border_style = Style::default()
        .fg(theme.colors.auto_sent_border)
        .add_modifier(Modifier::BOLD);

    vec![
        // Line 1: Feedback: <buffer>▋
        Line::from(vec![
            Span::styled("┃ ", border_style),
            Span::styled("Feedback: ", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled(
                buffer.to_string(),
                Style::default().fg(theme.colors.fg_primary),
            ),
            Span::styled("▋", Style::default().fg(theme.colors.fg_primary)),
            Span::styled(" ┃", border_style),
        ]),
        // Line 2: [Enter] send  [Esc] cancel
        Line::from(vec![
            Span::styled("┃ ", border_style),
            Span::styled("[Enter]", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled(" send  ", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled("[Esc]", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled(" cancel", Style::default().fg(theme.colors.fg_secondary)),
            Span::styled(" ┃", border_style),
        ]),
    ]
}

/// Render a blocked command notice in the chat stream.
#[allow(dead_code)]
pub fn render_blocked_notice_lines<'a>(reason: &str, theme: &'a Theme) -> Vec<Line<'a>> {
    let border_style = Style::default()
        .fg(theme.colors.error_border)
        .add_modifier(Modifier::BOLD);

    vec![Line::from(vec![
        Span::styled("┃ ", border_style),
        Span::styled(
            format!("{} ", crate::domain::models::visual::symbols::ERROR),
            Style::default()
                .fg(theme.colors.error)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            reason.to_string(),
            Style::default().fg(theme.colors.fg_primary),
        ),
        Span::styled(" ┃", border_style),
    ])]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::tool_call::ApprovalSource;

    #[test]
    fn test_feedback_input_renders_two_lines() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_feedback_input_lines("don't delete", &theme);
        assert_eq!(lines.len(), 2);
        let line1: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line1.contains("Feedback:"));
        assert!(line1.contains("don't delete"));
        assert!(line1.contains("▋"), "cursor marker missing");
        let line2: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line2.contains("[Enter]"));
        assert!(line2.contains("[Esc]"));
    }

    #[test]
    fn test_blocked_notice_renders_one_line() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let lines = render_blocked_notice_lines("Command blocked: rm -rf /", &theme);
        assert_eq!(lines.len(), 1);
        let content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("✗"));
        assert!(content.contains("Command blocked"));
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn foreground() -> ApprovalSource {
        ApprovalSource::ForegroundTurn {
            conversation_id: "c1".into(),
        }
    }

    /// `M01`/`M02`: the card shows exact field rows, never a JSON prefix, and
    /// T5's marker whenever rows past the fourth exist.
    #[test]
    fn card_shows_exact_rows_then_the_more_marker() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let input = serde_json::json!({
            "command": "cargo test\nline two\nline three\nline four\nLATE-SUFFIX",
        });
        let source = foreground();
        let card = PermissionCard {
            source: &source,
            tool_name: "Bash",
            risk: ToolRisk::Elevated,
            input: ExactInput::Ready(&input),
            queue_len: 2,
        };
        let rows = text(&render_permission_card_lines(&card, 80, &theme));
        assert_eq!(rows[0], "┃ Bash  [2 more queued]");
        assert_eq!(rows[1], "┃ │ command: cargo test");
        assert_eq!(rows[4], "┃ │ line four");
        assert_eq!(rows[5], "┃ … more not shown — [r] Review first");
        assert!(!rows.iter().any(|row| row.contains("LATE-SUFFIX")));
        assert!(!rows.iter().any(|row| row.contains('{')), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("[r] Review")));
        let review = render_permission_review(&card, 80, &theme);
        let body = text(&review.body);
        assert_eq!(body.last().map(String::as_str), Some("│ LATE-SUFFIX"));
        assert_eq!(
            text(&review.header)[0],
            "Exact tool input (not run yet) · Bash"
        );
        assert!(text(&review.footer).concat().contains("[Esc] Back"));
    }

    /// `M07` / `A5`: at the 60-column floor every wrapped continuation row is
    /// delimited and embedded controls and chrome look-alikes are inert text.
    #[test]
    fn every_physical_row_is_delimited_at_the_floor() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let input = serde_json::json!({
            "file_path": "/w/a.txt",
            "content": format!("{}\n[y] Allow\u{1b}[2J ┃ fake", "z".repeat(170)),
        });
        let source = foreground();
        let card = PermissionCard {
            source: &source,
            tool_name: "mcp__filesystem_server_with_a_long_name__write",
            risk: ToolRisk::Standard,
            input: ExactInput::Ready(&input),
            queue_len: 0,
        };
        let review = render_permission_review(&card, 60, &theme);
        let body = text(&review.body);
        assert!(body.iter().all(|row| row.starts_with("│ ")), "{body:?}");
        assert!(body.iter().all(|row| !row.chars().any(char::is_control)));
        assert!(
            body.iter().any(|row| row == "│ [y] Allow ┃ fake"),
            "{body:?}"
        );
        let card_rows = text(&render_permission_card_lines(&card, 60, &theme));
        assert!(card_rows.iter().all(|row| row.starts_with("┃ ")));
    }

    /// An attached card before completion shows the summary and the receiving
    /// line, never body rows.
    #[test]
    fn receiving_card_shows_summary_and_receiving_line() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let source = foreground();
        let card = PermissionCard {
            source: &source,
            tool_name: "Write",
            risk: ToolRisk::Standard,
            input: ExactInput::Receiving {
                summary: "/w/big.bin…",
            },
            queue_len: 0,
        };
        let rows = text(&render_permission_card_lines(&card, 80, &theme));
        assert_eq!(rows[1], "┃ │ /w/big.bin…");
        assert_eq!(rows[2], "┃ receiving exact input…");
    }

    /// A huge one-line summary never floods the card while parts arrive: at
    /// most the card's body rows, the cut marked, keys still reachable.
    #[test]
    fn receiving_summary_is_bounded_to_the_card_body_rows() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let source = foreground();
        let summary = "z".repeat(10_000);
        let card = PermissionCard {
            source: &source,
            tool_name: "Bash",
            risk: ToolRisk::Elevated,
            input: ExactInput::Receiving { summary: &summary },
            queue_len: 0,
        };
        let rows = text(&render_permission_card_lines(&card, 60, &theme));
        let summary_rows: Vec<_> = rows.iter().filter(|row| row.starts_with("┃ │ ")).collect();
        assert_eq!(summary_rows.len(), CARD_BODY_ROWS);
        assert!(summary_rows.last().unwrap().ends_with('…'));
        assert!(rows.iter().any(|row| row == "┃ receiving exact input…"));
        assert!(rows.iter().any(|row| row.contains("[r] Review")));
    }
}
