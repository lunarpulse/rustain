//! The rows of a tool permission as the decision frame paints them.
//!
//! Story 19.20c: the request shows the exact pending input (the shared `A2`
//! presentation) in the host-delimited body region, first ≤ 4 wrapped rows,
//! then TM-09's T5 marker; `[r] Review` opens the complete input. Story 21.6 6-B:
//! both hosts paint these rows inside the `Decision: Permission` frame (the frame
//! owns the border, the title and the scrolling), so every row here is borderless.

use ratatui::prelude::*;

use crate::adapters::tui::keymap::Context;
use crate::adapters::tui::keymap::render::{rail, rail_pair};
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::table_permission::PermissionCmd;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::delimited_body::{approval_body_rows, wrap_text};
use crate::adapters::tui::widgets::tool_block::display_tool_name;
use crate::domain::models::ToolRisk;
use crate::domain::models::tool_call::ApprovalSource;
use crate::domain::services::approval_presentation::{
    BODY_DELIMITER, CARD_BODY_ROWS, MORE_MARKER, body_lines,
};
use crate::domain::services::peer_text::sanitize_peer_text_line;

/// What a deciding surface holds of a request's exact input.
#[derive(Clone, Copy, Debug)]
pub enum ExactInput<'a> {
    /// The complete exact pending value.
    Ready(&'a serde_json::Value),
    /// Attached writer only: the exact input is still arriving in parts;
    /// `summary` is the daemon's one-line per-tool summary.
    Receiving { summary: &'a str },
}

/// One pending tool approval as the frame renders it.
#[derive(Clone, Copy, Debug)]
pub struct PermissionCard<'a> {
    pub source: &'a ApprovalSource,
    pub tool_name: &'a str,
    pub risk: ToolRisk,
    pub input: ExactInput<'a>,
}

/// The three regions of a permission frame body: the pinned header, the scrollable
/// body and the key rail. Review fills the same regions with the complete input.
#[derive(Debug)]
pub struct FrameRows {
    pub header: Vec<Line<'static>>,
    pub body: Vec<Line<'static>>,
    pub rail: Vec<Line<'static>>,
}

/// Ellipses apply only to identity/source metadata, never to exact input.
fn bounded_metadata(text: &str, width: usize, limit: usize) -> Vec<String> {
    let (mut rows, more) = wrap_text(text, width.saturating_sub(1).max(1), limit);
    if more && let Some(last) = rows.last_mut() {
        last.push('…');
    }
    rows
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
fn key_rows(items: &[(String, String, Color)], width: usize, label: Style) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cells = usize::MAX;
    for (key, text, color) in items {
        let item_cells = geom::cells(key) + 1 + geom::cells(text);
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
        row.push(Span::styled(key.clone(), Style::default().fg(*color)));
        row.push(Span::styled(format!(" {text}"), label));
        cells += item_cells;
    }
    rows.into_iter().map(Line::from).collect()
}

fn delimited_rows(
    tool_name: &str,
    input: &serde_json::Value,
    width: usize,
    limit: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, bool) {
    let lines = body_lines(tool_name, input);
    let text_width = width.saturating_sub(BODY_DELIMITER.chars().count());
    let (rows, more) = approval_body_rows(&lines, text_width, limit);
    let delimiter = Style::default().fg(theme.colors.accent);
    let label = Style::default()
        .fg(theme.colors.fg_secondary)
        .add_modifier(Modifier::BOLD);
    let value = Style::default().fg(theme.colors.fg_primary);
    let rendered = rows
        .into_iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(3);
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

fn receiving_rows(summary: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let text_width = width.saturating_sub(BODY_DELIMITER.chars().count()).max(1);
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
            Line::from(vec![
                Span::styled(BODY_DELIMITER, delimiter),
                Span::styled(row, value),
            ])
        })
        .collect();
    lines.push(Line::from(Span::styled(
        "receiving exact input…",
        Style::default().fg(theme.colors.fg_secondary),
    )));
    lines
}

/// Story 19.20c `A2`/`A5` — the request at `width` cells: the header
/// (`[source prefix] <Tool>`), the first ≤ 4 delimited body rows of the exact input,
/// T5's marker whenever more exists, and the decision keys with `[r] Review`
/// always offered. `keys_live` is false while the frame's inert window runs (the
/// keys paint muted). Exact input is never truncated in Review; long header/source
/// metadata is marked with an ellipsis.
pub fn card_rows(
    card: &PermissionCard<'_>,
    width: u16,
    theme: &Theme,
    keys_live: bool,
) -> FrameRows {
    let width = usize::from(width).max(8);
    let secondary = Style::default().fg(theme.colors.fg_secondary);
    let colour = |live: Color| {
        if keys_live {
            live
        } else {
            theme.colors.fg_muted
        }
    };
    let prefix = source_prefix(card.source);
    let tool = tool_label(card.tool_name);
    let header_text = sanitize_peer_text_line(&format!("{prefix}{tool}")).into_owned();
    let prefix_len = sanitize_peer_text_line(&prefix).len();
    let header = bounded_metadata(&header_text, width, 2)
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let tool_style = Style::default()
                .fg(theme.colors.tool_name)
                .add_modifier(Modifier::BOLD);
            if index == 0 && prefix_len > 0 && prefix_len <= row.len() {
                let (head, tail) = row.split_at(prefix_len);
                Line::from(vec![
                    Span::styled(
                        head.to_owned(),
                        Style::default().fg(theme.colors.subagent_attribution),
                    ),
                    Span::styled(tail.to_owned(), tool_style),
                ])
            } else {
                Line::from(Span::styled(row, tool_style))
            }
        })
        .collect();

    let mut body = Vec::new();
    match card.input {
        ExactInput::Ready(input) => {
            let (rows, more) = delimited_rows(card.tool_name, input, width, CARD_BODY_ROWS, theme);
            body.extend(rows);
            if more {
                for row in wrap_text(MORE_MARKER, width, usize::MAX).0 {
                    body.push(Line::from(Span::styled(row, secondary)));
                }
            }
        }
        ExactInput::Receiving { summary } => body.extend(receiving_rows(summary, width, theme)),
    }

    let mut rail_rows = key_rows(
        &[
            (
                rail(Context::Permission, PermissionCmd::AllowOnce),
                "Allow".to_owned(),
                colour(theme.colors.success),
            ),
            (
                rail(Context::Permission, PermissionCmd::AllowSession),
                "Session".to_owned(),
                colour(theme.colors.accent),
            ),
            (
                rail(Context::Permission, PermissionCmd::AllowAlways),
                always_label(card.tool_name),
                colour(theme.colors.warning),
            ),
        ],
        width,
        secondary,
    );
    rail_rows.extend(key_rows(
        &[
            (
                rail(Context::Permission, PermissionCmd::Deny),
                "Deny".to_owned(),
                colour(theme.colors.error),
            ),
            (
                rail(Context::Permission, PermissionCmd::DenyWithFeedback),
                "Deny + feedback".to_owned(),
                colour(theme.colors.auto_sent_border),
            ),
            (
                rail(Context::Permission, PermissionCmd::Review),
                "Review".to_owned(),
                colour(theme.colors.accent),
            ),
        ],
        width,
        secondary,
    ));
    FrameRows {
        header,
        body,
        rail: rail_rows,
    }
}

/// Story 19.20c `A4`/`A5` — the Review reader at `width` cells: heading
/// `Exact tool input (not run yet) · <Tool>`, the source/risk row, every delimited
/// body row (wrapped, nothing elided), and the footer keys.
pub fn review_rows(
    card: &PermissionCard<'_>,
    width: u16,
    theme: &Theme,
    keys_live: bool,
) -> FrameRows {
    let width = usize::from(width).max(8);
    let primary = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::BOLD);
    let secondary = Style::default().fg(theme.colors.fg_secondary);
    let colour = |live: Color| {
        if keys_live {
            live
        } else {
            theme.colors.fg_muted
        }
    };
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
            delimited_rows(card.tool_name, input, width, usize::MAX, theme).0
        }
        ExactInput::Receiving { summary } => receiving_rows(summary, width, theme),
    };
    let rail_rows = key_rows(
        &[
            (
                rail(Context::Permission, PermissionCmd::AllowOnce),
                "Allow".to_owned(),
                colour(theme.colors.success),
            ),
            (
                rail(Context::Permission, PermissionCmd::Deny),
                "Deny".to_owned(),
                colour(theme.colors.error),
            ),
            (
                rail(Context::Permission, PermissionCmd::DenyWithFeedback),
                "Deny + feedback".to_owned(),
                colour(theme.colors.auto_sent_border),
            ),
            (
                rail(Context::Decision, OverlayCmd::PaneEsc),
                "Back".to_owned(),
                colour(theme.colors.fg_secondary),
            ),
            (
                rail_pair(
                    Context::Decision,
                    OverlayCmd::PaneScrollUp,
                    OverlayCmd::PaneScrollDown,
                ),
                "scroll".to_owned(),
                colour(theme.colors.fg_secondary),
            ),
        ],
        width,
        secondary,
    );
    FrameRows {
        header,
        body,
        rail: rail_rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `M01`/`M02`: the request shows exact field rows, never a JSON prefix, and
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
        };
        let rows = card_rows(&card, 80, &theme, true);
        assert_eq!(text(&rows.header), ["Bash"]);
        let body = text(&rows.body);
        assert_eq!(body[0], "│ command: cargo test");
        assert_eq!(body[3], "│ line four");
        assert_eq!(
            body[4],
            format!(
                "… more not shown — {} Review first",
                rail(Context::Permission, PermissionCmd::Review)
            )
        );
        assert!(!body.iter().any(|row| row.contains("LATE-SUFFIX")));
        assert!(!body.iter().any(|row| row.contains('{')), "{body:?}");
        assert_eq!(
            text(&rows.rail),
            [
                "[y] Allow  [s] Session  [a] Always",
                "[n] Deny  [f] Deny + feedback  [r] Review"
            ]
        );
        let review = review_rows(&card, 80, &theme, true);
        assert_eq!(
            text(&review.body).last().map(String::as_str),
            Some("│ LATE-SUFFIX")
        );
        assert_eq!(
            text(&review.header)[0],
            "Exact tool input (not run yet) · Bash"
        );
        assert_eq!(
            text(&review.rail),
            ["[y] Allow  [n] Deny  [f] Deny + feedback  [Esc] Back  PgUp/PgDn scroll"]
        );
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
        };
        let review = review_rows(&card, 58, &theme, true);
        let body = text(&review.body);
        assert!(body.iter().all(|row| row.starts_with("│ ")), "{body:?}");
        assert!(body.iter().all(|row| !row.chars().any(char::is_control)));
        assert!(
            body.iter().any(|row| row == "│ [y] Allow ┃ fake"),
            "{body:?}"
        );
        let rows = card_rows(&card, 58, &theme, true);
        let shown: Vec<_> = text(&rows.body)
            .into_iter()
            .filter(|row| !row.starts_with("… more"))
            .collect();
        assert!(shown.iter().all(|row| row.starts_with("│ ")), "{shown:?}");
    }

    /// An attached request before completion shows the summary and the receiving
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
        };
        let rows = text(&card_rows(&card, 80, &theme, true).body);
        assert_eq!(rows[0], "│ /w/big.bin…");
        assert_eq!(rows[1], "receiving exact input…");
    }

    /// A huge one-line summary never floods the frame while parts arrive: at
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
        };
        let all = card_rows(&card, 58, &theme, true);
        let rows = text(&all.body);
        let summary_rows: Vec<_> = rows.iter().filter(|row| row.starts_with("│ ")).collect();
        assert_eq!(summary_rows.len(), CARD_BODY_ROWS);
        assert!(summary_rows.last().unwrap().ends_with('…'));
        assert!(rows.iter().any(|row| row == "receiving exact input…"));
        assert!(text(&all.rail).concat().contains("[r] Review"));
    }

    /// The inert window paints every key muted; the live rail uses the live colours.
    #[test]
    fn inert_keys_paint_muted() {
        let theme = crate::adapters::tui::theme::Theme::dark();
        let input = serde_json::json!({"command": "ls"});
        let source = foreground();
        let card = PermissionCard {
            source: &source,
            tool_name: "Bash",
            risk: ToolRisk::Standard,
            input: ExactInput::Ready(&input),
        };
        let key_colour = |rows: &FrameRows| rows.rail[0].spans[0].style.fg;
        assert_eq!(
            key_colour(&card_rows(&card, 80, &theme, false)),
            Some(theme.colors.fg_muted)
        );
        assert_eq!(
            key_colour(&card_rows(&card, 80, &theme, true)),
            Some(theme.colors.success)
        );
    }
}
