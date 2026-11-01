use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr as _;

use crate::adapters::tui::decision_pane;
use crate::adapters::tui::state::{DailyBudgetState, LogAwareness};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::chat_pane::virtual_scroll::offset_to_message_index;
use crate::adapters::tui::widgets::model_selector::humanize_ctx;
use crate::domain::models::{ActiveProfileSnapshot, PermissionMode, StatusState, UsageInfo};
use crate::infrastructure::clock_util::now_unix;

/// Render the status bar with model name, current status, scroll position, and permission mode.
// Covers: FR38, UX-DR76, UX-DR93
pub fn render(
    frame: &mut Frame,
    area: Rect,
    model: &str,
    provider_status: Option<&str>,
    status: &StatusState,
    theme: &Theme,
    scroll_offset: usize,
    message_boundaries: &[usize],
    total_content_height: usize,
    viewport_height: u16,
    permission_mode: PermissionMode,
    token_usage: Option<&UsageInfo>,
    context_window: u32,
    has_project_context: bool,
    session_title: Option<&str>,
    multiline_mode: bool,
    context_injection_on: bool,
    injected_tokens: u32,
    current_hint: Option<&str>,
    active_skill_count: usize,
    active_agent_name: Option<&str>,
    pending_plan_reminder_at_turn: Option<u32>,
    drill_down_breadcrumb: Option<&str>,
    pinned_active: bool,
    daily_budget: Option<&DailyBudgetState>,
    active_profile: Option<&ActiveProfileSnapshot>,
    density_mode: crate::domain::models::visual::DensityMode,
    tab_read_only: bool,
    attached: Option<&crate::adapters::tui::state::AttachInfo>,
    log_awareness: LogAwareness,
    badges: impl Into<NoticeBadges>,
) {
    let badges: NoticeBadges = badges.into();
    let unseen_notices = badges.unseen;
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Story 21.1 A12: YOLO is a status FACT of the permission mode, not a flash.
    // It shows while nothing more specific (streaming, executing, retrying) does.
    let status_text =
        if permission_mode == PermissionMode::Yolo && matches!(status, StatusState::Idle) {
            YOLO_FACT.to_string()
        } else {
            status.display_text()
        };
    let fg = theme.colors.status_fg;
    let sep = " │ ";

    // Build left side: model [ctx] │ mode │ tokens │ status
    // Spec layout: "sonnet-4-6 [ctx] │ normal │ ↑1.2k ↓3.4k │ Ready"
    let model_label = match provider_status {
        Some(provider_id) => {
            let combined = format!(" {}/{}", provider_id, model);
            if has_project_context {
                format!("{} [ctx]", combined)
            } else {
                combined
            }
        }
        None => {
            if has_project_context {
                format!(" {} [ctx]", model)
            } else {
                format!(" {}", model)
            }
        }
    };
    let mut left_spans: Vec<Span> = Vec::new();

    // S16.8 AC15: Persistent anchor indicator when Pinned (LEFT slot).
    if pinned_active {
        left_spans.push(Span::styled(
            " ⚓ ".to_string(),
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }

    left_spans.push(Span::styled(model_label, Style::default().fg(fg)));

    // Story 8.4b AC-5: Density mode indicator chip [F]/[M]/[D]
    {
        use crate::domain::models::visual::DensityMode;
        let indicator = density_mode.indicator_char();
        let indicator_style = match density_mode {
            DensityMode::Focus => Style::default().fg(theme.colors.fg_muted),
            DensityMode::Monitor => Style::default().fg(theme.colors.accent),
            DensityMode::Dashboard => Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        };
        left_spans.push(Span::styled(format!(" [{}]", indicator), indicator_style));
    }

    if tab_read_only {
        left_spans.push(Span::styled(
            " [READ-ONLY]".to_string(),
            Style::default()
                .fg(theme.colors.warning)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Story 12.2c AC1/AC6 — attach-mode indicator. Shown only for an attached
    // client (`run_attached`); the local TUI passes `None`. When this client was
    // granted read-only (another client holds the writer slot) the segment is the
    // dimmed never-silent `read-only — another client holds write` (AC6 #2);
    // otherwise a plain `attached` chip.
    if let Some(info) = attached {
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        if info.read_only {
            left_spans.push(Span::styled(
                "read-only — another client holds write".to_string(),
                theme.typography.meta.fg(theme.colors.fg_muted),
            ));
        } else {
            left_spans.push(Span::styled(
                "attached".to_string(),
                Style::default().fg(theme.colors.accent),
            ));
        }
        if info.channel_count > 1 {
            left_spans.push(Span::styled(
                format!(" ({} channels)", info.channel_count),
                theme.typography.meta.fg(theme.colors.fg_muted),
            ));
        }
    }

    if let Some(breadcrumb) = drill_down_breadcrumb {
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(
            breadcrumb.to_string(),
            Style::default().fg(theme.colors.fg_muted),
        ));
    }

    // Session title (after model, if restored session)
    if let Some(title) = session_title {
        let display = if title.is_empty() { "Untitled" } else { title };
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(display.to_string(), Style::default().fg(fg)));
    }

    // Permission mode (second segment)
    let mode_text = match permission_mode {
        PermissionMode::Plan => "PLAN",
        PermissionMode::Normal => "Normal",
        PermissionMode::AutoEdit => "AUTOEDIT",
        PermissionMode::Yolo => "YOLO",
    };
    left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
    left_spans.push(match permission_mode {
        PermissionMode::Plan => {
            Span::styled(mode_text.to_string(), Style::default().fg(Color::Blue))
        }
        PermissionMode::Normal => Span::styled(mode_text.to_string(), Style::default().fg(fg)),
        PermissionMode::AutoEdit => Span::styled(
            mode_text.to_string(),
            Style::default().fg(theme.colors.accent),
        ),
        PermissionMode::Yolo => Span::styled(
            mode_text.to_string(),
            Style::default()
                .fg(Color::White)
                .bg(theme.colors.status_yolo_warning)
                .add_modifier(Modifier::BOLD),
        ),
    });

    // Plan-mode reminder chip ( Story 6-0d AC5/AC8 )
    if permission_mode == PermissionMode::Plan {
        if let Some(turn) = pending_plan_reminder_at_turn {
            left_spans.push(Span::styled(
                format!("{}⟳ plan-reminder t+{}", sep, turn),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    // Token usage (third segment)
    if let Some(usage) = token_usage {
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(
            format_token_usage(usage),
            Style::default().fg(fg),
        ));
    }

    // Story 11.4 AC3: injected context token cost (shown when > 0).
    if injected_tokens > 0 {
        left_spans.push(Span::styled(
            format!("{}+{} ctx", sep, format_token_count(injected_tokens)),
            Style::default().fg(theme.colors.accent),
        ));
    }

    // Daily budget segment (Story 7.5 AC5). Suppressed when paused
    // (`unix_now <= dismissed_until_unix`).
    // The segment only appears at ≥80% utilization (yellow/red).
    // Below 80% the budget is invisible — users see utilization in the
    // usage panel (Ctrl+X, U) instead. This is intentional: the status bar
    // is reserved for actionable warnings, not informational displays.
    if let Some(b) = daily_budget {
        if b.limit_usd > 0.0 && now_unix() > b.dismissed_until_unix {
            let pct = b.percent();
            // Only render when ≥80% (yellow) or ≥100% (red) per AC5.
            if pct >= 80 {
                let color = if pct >= 100 {
                    theme.colors.error
                } else {
                    theme.colors.warning
                };
                left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
                left_spans.push(Span::styled(
                    format!(
                        "budget: ${:.2}/${:.2} ({}%)",
                        b.spent_today_usd, b.limit_usd, pct
                    ),
                    Style::default().fg(color),
                ));
            }
        }
    }

    // Context window ratio (Story 7.4 AC1/AC2)
    if context_window > 0 {
        let used = token_usage.map_or(0, |u| u.input_tokens);
        let pct = used
            .saturating_mul(100)
            .checked_div(context_window)
            .unwrap_or(0);
        let ctx_color = if pct >= 95 {
            theme.colors.error
        } else if pct >= 80 {
            theme.colors.warning
        } else {
            theme.colors.status_fg
        };
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(
            format!(
                "ctx: {}/{} ({}%)",
                humanize_ctx(used),
                humanize_ctx(context_window),
                pct
            ),
            Style::default().fg(ctx_color),
        ));
    }

    // Status (fourth segment — rightmost, most dynamic)
    left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
    left_spans.push(Span::styled(
        status_text,
        Style::default().fg(if status.is_active() {
            theme.colors.status_streaming
        } else {
            fg
        }),
    ));

    // ML mode indicator
    // Covers: UX-DR76
    if multiline_mode {
        left_spans.push(Span::styled(
            format!("{}[ML]", sep),
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Story 11.4 (AC7) — memory-injection toggle indicator. A DISTINCT `mem: off`
    // segment (NOT overloading the `ctx: N/M (P%)` window ratio above — Q3).
    // Shown only when OFF (the notable state); ON is the default, left unannotated.
    if !context_injection_on {
        left_spans.push(Span::styled(
            format!("{}mem: off", sep),
            Style::default()
                .fg(theme.colors.warning)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Active skill count (AC12: suppressed when zero)
    if active_skill_count > 0 {
        left_spans.push(Span::styled(
            format!("{}Skills: {} active", sep, active_skill_count),
            Style::default().fg(theme.colors.accent),
        ));
    }

    // Active agent (Story 5.4 AC7: show name when active, suppress when none)
    if let Some(name) = active_agent_name {
        // 24 display cells, the `…` marker included in the budget, cut on a
        // whole-grapheme boundary.
        let truncated = geom::ellipsize(name, 24, "…").into_owned();
        left_spans.push(Span::styled(
            format!("{}Agent: {}", sep, truncated),
            Style::default().fg(theme.colors.accent),
        ));
    }

    // AC4: Show scroll position indicator when scrolled
    if scroll_offset > 0 && !message_boundaries.is_empty() {
        let (current, total) = offset_to_message_index(
            scroll_offset,
            viewport_height,
            message_boundaries,
            total_content_height,
        );
        left_spans.push(Span::styled(
            format!(" │ msg {}/{}", current, total),
            Style::default().fg(fg),
        ));
    }

    if let Some(profile) = active_profile {
        let profile_label = if profile.preview {
            format!("{} (preview)", profile.name)
        } else {
            profile.name.clone()
        };
        left_spans.push(Span::styled(sep.to_string(), Style::default().fg(fg)));
        left_spans.push(Span::styled(
            "█".to_string(),
            Style::default().fg(Color::Indexed(profile.identity_color.0)),
        ));
        left_spans.push(Span::styled(
            format!(" {}", profile_label),
            Style::default().fg(fg),
        ));
    }

    // Story 19.16g — the transparency-log reminder owns the right edge. Its
    // whole display width (plus one separating space when room permits) is
    // reserved BEFORE the optional hint and the left content are laid out,
    // so neither can overwrite it; they clip on the left region instead. A bar
    // narrower than the segment omits it whole — never a partial number.
    let area = match log_reminder_text(log_awareness) {
        Some(reminder) if usize::from(area.width) >= reminder.width() => {
            let reserved = (reminder.width() + 1).min(usize::from(area.width)) as u16;
            let left = Rect {
                width: area.width - reserved,
                ..area
            };
            let right = Rect {
                x: area.x + left.width,
                width: reserved,
                ..area
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    reminder,
                    theme.typography.meta.fg(theme.colors.fg_muted),
                )))
                .alignment(Alignment::Right)
                .style(Style::default().bg(theme.colors.status_bg)),
                right,
            );
            left
        }
        _ => area,
    };

    // Story 21.1 S2 AC10 — `Needs you (N) ^X q` while decisions wait. A question
    // nobody sees is lost, so like the log reminder it is reserved at the right
    // edge BEFORE the left content is laid out: a long model/token/context row
    // clips on the left instead of hiding it (omitted whole only when the bar is
    // narrower than the cue itself).
    let area = match decision_pane::status_cue(badges.decisions) {
        Some(cue) if usize::from(area.width) >= geom::cells(&cue) => {
            let reserved = (geom::cells(&cue) + 1).min(usize::from(area.width)) as u16;
            let left = Rect {
                width: area.width - reserved,
                ..area
            };
            let right = Rect {
                x: area.x + left.width,
                width: reserved,
                ..area
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    cue,
                    Style::default()
                        .fg(theme.colors.accent)
                        .add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Right)
                .style(Style::default().bg(theme.colors.status_bg)),
                right,
            );
            left
        }
        _ => area,
    };
    // Contextual hint and the Story 21.1 `Log !N` notice badge: right-aligned in
    // the remaining space (UX-DR93, UX-DR96). Admission order: the `log: N`
    // reminder (reserved above) first, then the hint, then the badge — the badge
    // is only painted when it still fits; the hint is the first thing truncated
    // if the terminal is too narrow.
    let left_width = left_spans
        .iter()
        .map(|s| geom::cells(&s.content))
        .sum::<usize>();
    let bar_width = area.width as usize;
    let hint = current_hint.filter(|hint| left_width + 1 + geom::cells(hint) <= bar_width);
    let hint_width = hint.map_or(0, geom::cells);
    let badge = notice_badge_text(unseen_notices).filter(|badge| {
        let gap = usize::from(hint.is_some());
        left_width + 1 + geom::cells(badge) + gap + hint_width <= bar_width
    });
    if hint.is_some() || badge.is_some() {
        let mut right: Vec<Span> = Vec::new();
        let mut right_width = hint_width;
        if let Some(badge) = badge {
            right_width += geom::cells(&badge) + usize::from(hint.is_some());
            right.push(Span::styled(
                badge,
                Style::default()
                    .fg(theme.colors.warning)
                    .add_modifier(Modifier::BOLD),
            ));
            if hint.is_some() {
                right.push(Span::raw(" "));
            }
        }
        if let Some(hint) = hint {
            right.push(Span::styled(
                hint,
                theme.typography.hint.fg(theme.colors.text_hint),
            ));
        }
        let padding = bar_width - left_width - right_width;
        let mut spans = left_spans;
        spans.push(Span::raw(" ".repeat(padding)));
        spans.extend(right);
        let widget =
            Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.colors.status_bg));
        return frame.render_widget(widget, area);
    }

    let line = Line::from(left_spans);
    let widget = Paragraph::new(line).style(Style::default().bg(theme.colors.status_bg));
    frame.render_widget(widget, area);
}

/// What the status row's right edge may add beside the hint: the unseen
/// warning/error badge and the waiting-decision cue. A bare `u64` is a badge
/// with no decisions (the attached client, which has no decision producers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoticeBadges {
    pub unseen: u64,
    pub decisions: usize,
}

impl From<u64> for NoticeBadges {
    fn from(unseen: u64) -> Self {
        Self {
            unseen,
            decisions: 0,
        }
    }
}

/// Story 21.1 A12: the status fact painted while YOLO is the permission mode.
pub const YOLO_FACT: &str = "⚠ YOLO mode active — all tools auto-approved";

/// Story 21.1 AC5 — the unseen warning/error badge (`Log !N`, capped `99+`).
/// `None` hides it: it appears only while something still needs a look.
fn notice_badge_text(unseen: u64) -> Option<String> {
    match unseen {
        0 => None,
        1..=99 => Some(format!("Log !{unseen}")),
        _ => Some("Log !99+".to_owned()),
    }
}

/// Story 19.16g — the reminder's ceiling. The count still moves with journal
/// activity; the cap bounds its magnitude, not peer influence.
pub const LOG_REMINDER_CAP: usize = 99;

/// Story 19.16g — the segment text: locally authored ASCII and a bounded
/// count, nothing else (no peer, item, task, timestamp, or kind). `None`
/// hides the segment and its separator.
fn log_reminder_text(awareness: LogAwareness) -> Option<String> {
    match awareness {
        LogAwareness::Hidden | LogAwareness::Unseen(0) => None,
        LogAwareness::Unseen(count) if count > LOG_REMINDER_CAP => {
            Some(format!("log: {LOG_REMINDER_CAP}+"))
        }
        LogAwareness::Unseen(count) => Some(format!("log: {count}")),
        LogAwareness::Unavailable => Some("log: ?".to_owned()),
    }
}

/// Format token counts compactly: raw numbers below 1000, `Xk` suffix above.
///
/// Story 7.5: promoted from `fn` to `pub fn` for reuse by `usage_panel.rs`.
pub fn format_token_count(count: u32) -> String {
    if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        count.to_string()
    }
}

/// Format token usage as `↑{input} ↓{output}`.
pub fn format_token_usage(usage: &UsageInfo) -> String {
    format!(
        "↑{} ↓{}",
        format_token_count(usage.input_tokens),
        format_token_count(usage.output_tokens)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::StatusState;
    use crate::domain::models::visual::DensityMode;

    #[test]
    fn test_format_token_count_small() {
        assert_eq!(format_token_count(500), "500");
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
    }

    #[test]
    fn test_format_token_count_large() {
        assert_eq!(format_token_count(1000), "1.0k");
        assert_eq!(format_token_count(1200), "1.2k");
        assert_eq!(format_token_count(3400), "3.4k");
        assert_eq!(format_token_count(10000), "10.0k");
    }

    #[test]
    fn test_format_token_usage() {
        let usage = UsageInfo {
            input_tokens: 1200,
            output_tokens: 3400,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        };
        assert_eq!(format_token_usage(&usage), "↑1.2k ↓3.4k");
    }

    // ── Story 7.5 AC2 + AC5: fixture-driven status-bar render tests ─────────
    // These tests use ratatui's TestBackend to verify the rendered buffer
    // contains the expected `↑/↓` token segment (AC2 — already shipping from 7-4)
    // and `budget:` segment (AC5 — new in 7-5) substrings.

    use crate::adapters::tui::state::DailyBudgetState;
    use crate::adapters::tui::theme::Theme;
    use crate::domain::models::PermissionMode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn buffer_text(t: &Terminal<TestBackend>) -> String {
        let buf = t.backend().buffer();
        let mut s = String::new();
        let term_size = t.size().unwrap();
        for y in 0..term_size.height {
            for x in 0..term_size.width {
                if let Some(cell) = buf.cell((x, y)) {
                    s.push_str(cell.symbol());
                }
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn status_bar_renders_token_arrows_segment() {
        let backend = TestBackend::new(200, 1);
        let mut t = Terminal::new(backend).unwrap();
        let theme = Theme::dark();
        let usage = UsageInfo {
            input_tokens: 1200,
            output_tokens: 3400,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        };
        t.draw(|frame| {
            render(
                frame,
                frame.area(),
                "sonnet-4-6",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                Some(&usage),
                200_000, // context_window
                false,
                None,
                false,
                true,
                0, // injected_tokens
                None,
                0,
                None,
                None,
                None,
                false,
                None,
                None,
                DensityMode::Focus,
                false,
                None,
                LogAwareness::Hidden,
                0,
            );
        })
        .unwrap();
        let txt = buffer_text(&t);
        assert!(txt.contains("↑1.2k ↓3.4k"), "↑/↓ segment missing: {txt}");
        assert!(txt.contains("ctx:"), "ctx: segment missing: {txt}");
    }

    #[test]
    fn status_bar_renders_budget_warning_at_85_percent() {
        let backend = TestBackend::new(200, 1);
        let mut t = Terminal::new(backend).unwrap();
        let theme = Theme::dark();
        let usage = UsageInfo {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        };
        let db = DailyBudgetState {
            spent_today_usd: 4.25,
            limit_usd: 5.00,
            computed_at_ms: 0,
            dismissed_until_unix: 0,
        };
        t.draw(|frame| {
            render(
                frame,
                frame.area(),
                "sonnet-4-6",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                Some(&usage),
                0,
                false,
                None,
                false,
                true,
                0, // injected_tokens
                None,
                0,
                None,
                None,
                None,
                false,
                Some(&db),
                None,
                DensityMode::Focus,
                false,
                None,
                LogAwareness::Hidden,
                0,
            );
        })
        .unwrap();
        let txt = buffer_text(&t);
        assert!(
            txt.contains("budget: $4.25/$5.00 (85%)"),
            "85% budget segment missing: {txt}"
        );
    }

    #[test]
    fn status_bar_suppresses_budget_when_paused() {
        let backend = TestBackend::new(200, 1);
        let mut t = Terminal::new(backend).unwrap();
        let theme = Theme::dark();
        let usage = UsageInfo {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
        };
        let db = DailyBudgetState {
            spent_today_usd: 10.0,
            limit_usd: 5.00,
            computed_at_ms: 0,
            // far future → paused
            dismissed_until_unix: now_unix() + 86_400,
        };
        t.draw(|frame| {
            render(
                frame,
                frame.area(),
                "sonnet-4-6",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                Some(&usage),
                0,
                false,
                None,
                false,
                true,
                0, // injected_tokens
                None,
                0,
                None,
                None,
                None,
                false,
                Some(&db),
                None,
                DensityMode::Focus,
                false,
                None,
                LogAwareness::Hidden,
                0,
            );
        })
        .unwrap();
        let txt = buffer_text(&t);
        assert!(
            !txt.contains("budget:"),
            "paused budget should not render: {txt}"
        );
    }

    #[test]
    fn test_status_state_display_text() {
        assert_eq!(StatusState::Idle.display_text(), "Ready");
        assert_eq!(StatusState::Streaming.display_text(), "Streaming...");
        assert_eq!(
            StatusState::Executing {
                tool_name: "bash".to_string(),
                elapsed_ms: 500,
            }
            .display_text(),
            "Executing bash..."
        );
        assert_eq!(
            StatusState::Retrying {
                attempt: 2,
                max: 5,
                next_in_ms: 4000,
            }
            .display_text(),
            "Retrying (2/5) in 4.0s"
        );
    }

    #[test]
    fn test_status_state_is_active() {
        assert!(!StatusState::Idle.is_active());
        assert!(StatusState::Streaming.is_active());
        assert!(
            StatusState::Executing {
                tool_name: "test".to_string(),
                elapsed_ms: 0,
            }
            .is_active()
        );
        assert!(
            StatusState::Retrying {
                attempt: 1,
                max: 5,
                next_in_ms: 1000,
            }
            .is_active()
        );
    }

    #[test]
    fn status_bar_renders_read_only_badge() {
        let backend = TestBackend::new(200, 1);
        let mut t = Terminal::new(backend).unwrap();
        let theme = Theme::dark();
        t.draw(|frame| {
            render(
                frame,
                frame.area(),
                "sonnet-4-6",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                None,
                0,
                false,
                None,
                false,
                true,
                0, // injected_tokens
                None,
                0,
                None,
                None,
                None,
                false,
                None,
                None,
                DensityMode::Focus,
                true,
                None,
                LogAwareness::Hidden,
                0,
            );
        })
        .unwrap();
        let txt = buffer_text(&t);
        assert!(txt.contains("READ-ONLY"), "READ-ONLY badge missing: {txt}");
    }

    // Story 11.4 AC7: `mem: off` segment renders iff context injection is OFF;
    // it is distinct from the `ctx: N/M` window ratio (Q3).
    #[test]
    fn status_bar_renders_mem_off_when_injection_disabled() {
        fn draw(injection_on: bool) -> String {
            let backend = TestBackend::new(200, 1);
            let mut t = Terminal::new(backend).unwrap();
            let theme = Theme::dark();
            t.draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    "sonnet-4-6",
                    None,
                    &StatusState::Idle,
                    &theme,
                    0,
                    &[],
                    0,
                    20,
                    PermissionMode::Normal,
                    None,
                    200_000,
                    false,
                    None,
                    false,
                    injection_on,
                    0, // injected_tokens
                    None,
                    0,
                    None,
                    None,
                    None,
                    false,
                    None,
                    None,
                    DensityMode::Focus,
                    false,
                    None,
                    LogAwareness::Hidden,
                    0,
                );
            })
            .unwrap();
            buffer_text(&t)
        }

        let off = draw(false);
        assert!(
            off.contains("mem: off"),
            "expected `mem: off` segment: {off}"
        );
        let on = draw(true);
        assert!(
            !on.contains("mem: off"),
            "`mem: off` must NOT show when injection is on: {on}"
        );
    }

    // Story 11.4 AC3: injected token cost is shown when > 0.
    #[test]
    fn status_bar_renders_injected_token_cost_when_present() {
        let backend = TestBackend::new(200, 1);
        let mut t = Terminal::new(backend).unwrap();
        let theme = Theme::dark();
        t.draw(|frame| {
            render(
                frame,
                frame.area(),
                "sonnet-4-6",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                None,
                200_000,
                false,
                None,
                false,
                true,
                123, // injected_tokens
                None,
                0,
                None,
                None,
                None,
                false,
                None,
                None,
                DensityMode::Focus,
                false,
                None,
                LogAwareness::Hidden,
                0,
            );
        })
        .unwrap();
        let txt = buffer_text(&t);
        assert!(
            txt.contains("+123 ctx"),
            "expected `+123 ctx` segment: {txt}"
        );
    }

    // ── Story 19.16g — the transparency-log reminder segment ────────────────

    /// Render one real status bar row and return its cells.
    fn draw_reminder_bar(
        width: u16,
        model: &str,
        session_title: Option<&str>,
        hint: Option<&str>,
        density: DensityMode,
        awareness: LogAwareness,
    ) -> ratatui::buffer::Buffer {
        let mut t = Terminal::new(TestBackend::new(width, 1)).unwrap();
        let theme = Theme::dark();
        t.draw(|frame| {
            render(
                frame,
                frame.area(),
                model,
                Some("provider-with-a-long-id"),
                &StatusState::Streaming,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                None,
                200_000,
                true,
                session_title,
                true,
                false,
                0,
                hint,
                3,
                Some("a-very-long-agent-name-that-exceeds"),
                None,
                None,
                false,
                None,
                None,
                density,
                false,
                None,
                awareness,
                0,
            );
        })
        .unwrap();
        t.backend().buffer().clone()
    }

    fn row_text(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.width)
            .map(|x| buffer.cell((x, 0)).unwrap().symbol())
            .collect()
    }

    /// The last `segment.len()` cells, read cell by cell.
    fn right_edge(buffer: &ratatui::buffer::Buffer, cells: u16) -> String {
        let width = buffer.area.width;
        (width.saturating_sub(cells)..width)
            .map(|x| buffer.cell((x, 0)).unwrap().symbol())
            .collect()
    }

    #[test]
    fn unseen_count_is_bounded_and_hidden_at_zero() {
        // K01 / M01: exact, capped, unavailable, hidden — at the right edge.
        for (awareness, expected) in [
            (LogAwareness::Unseen(1), Some("log: 1")),
            (LogAwareness::Unseen(99), Some("log: 99")),
            (LogAwareness::Unseen(100), Some("log: 99+")),
            (LogAwareness::Unseen(150), Some("log: 99+")),
            (LogAwareness::Unavailable, Some("log: ?")),
            (LogAwareness::Unseen(0), None),
            (LogAwareness::Hidden, None),
        ] {
            let buffer =
                draw_reminder_bar(200, "sonnet-4-6", None, None, DensityMode::Focus, awareness);
            let row = row_text(&buffer);
            match expected {
                Some(segment) => {
                    assert_eq!(
                        right_edge(&buffer, segment.len() as u16 + 1),
                        format!(" {segment}"),
                        "{awareness:?}: {row}"
                    );
                    assert_eq!(row.matches("log:").count(), 1, "{row}");
                }
                None => assert!(!row.contains("log:"), "{awareness:?}: {row}"),
            }
        }
    }

    #[test]
    fn log_reminder_is_passive_in_every_density() {
        // AC2: muted meta styling on the status background, no added
        // emphasis, in Focus, Monitor and Dashboard alike.
        let theme = Theme::dark();
        let expected = theme.typography.meta.fg(theme.colors.fg_muted);
        for density in [
            DensityMode::Focus,
            DensityMode::Monitor,
            DensityMode::Dashboard,
        ] {
            let buffer = draw_reminder_bar(
                120,
                "sonnet-4-6",
                None,
                None,
                density,
                LogAwareness::Unseen(7),
            );
            assert_eq!(right_edge(&buffer, 6), "log: 7", "{density:?}");
            for x in 114..120 {
                let cell = buffer.cell((x, 0)).unwrap();
                assert_eq!(cell.fg, expected.fg.unwrap(), "{density:?}");
                assert_eq!(cell.bg, theme.colors.status_bg, "{density:?}");
                assert_eq!(
                    cell.modifier, expected.add_modifier,
                    "no emphasis beyond the meta token ({density:?})"
                );
            }
        }
    }

    #[test]
    fn unseen_segment_survives_narrow_and_unicode_prefixes() {
        // K07 / M07: the whole segment stays visible at every width, however
        // long (or wide) the left content; the optional hint yields first.
        let long_model = "claude-sonnet-4-6-with-an-extremely-long-model-identifier";
        let cjk_session = "한국어 세션 제목이 매우 길다 中文会话名称也很长";
        for width in [40u16, 60, 80, 120, 200] {
            for (awareness, segment) in [
                (LogAwareness::Unseen(7), "log: 7"),
                (LogAwareness::Unseen(150), "log: 99+"),
                (LogAwareness::Unavailable, "log: ?"),
            ] {
                for (model, session) in [(long_model, None), ("m", Some(cjk_session))] {
                    let buffer = draw_reminder_bar(
                        width,
                        model,
                        session,
                        Some("^X d detach"),
                        DensityMode::Focus,
                        awareness,
                    );
                    assert_eq!(
                        right_edge(&buffer, segment.len() as u16 + 1),
                        format!(" {segment}"),
                        "width {width}, {awareness:?}, {model}: {}",
                        row_text(&buffer)
                    );
                }
            }
        }
        // Exactly the segment's width: the segment alone, no separator.
        let exact = draw_reminder_bar(
            6,
            long_model,
            None,
            None,
            DensityMode::Focus,
            LogAwareness::Unseen(7),
        );
        assert_eq!(row_text(&exact), "log: 7");
        // Narrower than the segment: omitted whole, never a partial number.
        let narrow = draw_reminder_bar(
            5,
            "m",
            None,
            None,
            DensityMode::Focus,
            LogAwareness::Unseen(7),
        );
        assert!(!row_text(&narrow).contains('7'), "{}", row_text(&narrow));
        assert!(!row_text(&narrow).contains("log"), "{}", row_text(&narrow));
    }

    // ── Story 21.11 AC4/K4: display-width correctness ──────────────────────
    // A minimal real status row: the left side is ` {model} [F] │ Normal │
    // Ready` (22 cells + the model's cells), the hint is right-aligned in what
    // remains after the optional log reservation.

    const C: &str = "界";
    const G: &str = "👨\u{200d}👩\u{200d}👧";

    fn draw_minimal(
        width: u16,
        model: &str,
        hint: Option<&str>,
        agent: Option<&str>,
        awareness: LogAwareness,
    ) -> ratatui::buffer::Buffer {
        let mut t = Terminal::new(TestBackend::new(width, 1)).unwrap();
        let theme = Theme::dark();
        t.draw(|frame| {
            render(
                frame,
                frame.area(),
                model,
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                PermissionMode::Normal,
                None,
                0,
                false,
                None,
                false,
                true,
                0,
                hint,
                0,
                agent,
                None,
                None,
                false,
                None,
                None,
                DensityMode::Focus,
                false,
                None,
                awareness,
                0,
            );
        })
        .unwrap();
        t.backend().buffer().clone()
    }

    /// The row as painted glyphs; wide-glyph continuation cells are skipped.
    fn glyph_row(buffer: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        let mut x = 0u16;
        while x < buffer.area.width {
            let symbol = buffer.cell((x, 0)).unwrap().symbol();
            out.push_str(symbol);
            x += u16::try_from(geom::cells(symbol).max(1)).unwrap();
        }
        out
    }

    #[test]
    fn display_width_status_hint_respects_the_log_reservation_edge() {
        // 60 columns, `log: 7` reserved with its gap (7 cells): the left
        // region is 53 cells; beside `模型` (26) that leaves 26 hint cells.
        let cases: Vec<(String, bool)> = vec![
            ("h".repeat(26), true),
            ("h".repeat(27), false),
            (format!("{G}{}", "h".repeat(24)), true),
            (format!("{G}{}", "h".repeat(25)), false),
            (format!("{C}{}", "h".repeat(24)), true),
            (format!("{C}{}", "h".repeat(25)), false),
        ];
        for (hint, fits) in cases {
            let buffer = draw_minimal(60, "模型", Some(&hint), None, LogAwareness::Unseen(7));
            let row = glyph_row(&buffer);
            let trimmed = row.trim_end();
            assert_eq!(geom::cells(&row), 60, "{hint:?}: {row:?}");
            assert!(
                trimmed.ends_with(" log: 7"),
                "reserved segment intact: {row:?}"
            );
            if fits {
                assert!(
                    trimmed.ends_with(&format!("{hint} log: 7")),
                    "{hint:?} must end one gap before the segment: {row:?}"
                );
            } else {
                assert!(!row.contains("hh"), "{hint:?} must be omitted: {row:?}");
                assert!(!row.contains(G), "{hint:?}: {row:?}");
            }
        }
    }

    // ── Story 21.1 — YOLO status fact and the `Log !N` notice badge ────────

    /// Render one real row with an explicit permission mode, status and unseen
    /// notice count.
    fn draw_notice_bar(
        width: u16,
        status: &StatusState,
        mode: PermissionMode,
        hint: Option<&str>,
        awareness: LogAwareness,
        unseen_notices: u64,
    ) -> String {
        let mut t = Terminal::new(TestBackend::new(width, 1)).unwrap();
        let theme = Theme::dark();
        t.draw(|frame| {
            render(
                frame,
                frame.area(),
                "m",
                None,
                status,
                &theme,
                0,
                &[],
                0,
                20,
                mode,
                None,
                0,
                false,
                None,
                false,
                true,
                0,
                hint,
                0,
                None,
                None,
                None,
                false,
                None,
                None,
                DensityMode::Focus,
                false,
                None,
                awareness,
                unseen_notices,
            );
        })
        .unwrap();
        row_text(t.backend().buffer())
    }

    #[test]
    fn story21_1_yolo_is_a_status_fact_of_the_permission_mode() {
        // A12: the fact comes from `permission_mode`, never from a producer's
        // sticky flash — it shows while idle and yields to specific statuses.
        let idle = StatusState::Idle;
        let row = draw_notice_bar(
            120,
            &idle,
            PermissionMode::Yolo,
            None,
            LogAwareness::Hidden,
            0,
        );
        assert!(row.contains(YOLO_FACT), "{row}");
        assert!(!row.contains("Ready"), "{row}");

        let row = draw_notice_bar(
            120,
            &idle,
            PermissionMode::Normal,
            None,
            LogAwareness::Hidden,
            0,
        );
        assert!(!row.contains("YOLO"), "{row}");
        assert!(row.contains("Ready"), "{row}");

        let row = draw_notice_bar(
            120,
            &StatusState::Streaming,
            PermissionMode::Yolo,
            None,
            LogAwareness::Hidden,
            0,
        );
        assert!(row.contains("Streaming..."), "{row}");
        assert!(!row.contains(YOLO_FACT), "{row}");
    }

    #[test]
    fn story21_1_notice_badge_is_bounded_and_hidden_at_zero() {
        // AC5: `Log !N`, capped `99+`, absent while nothing is unseen.
        for (unseen, expected) in [
            (0u64, None),
            (1, Some("Log !1")),
            (99, Some("Log !99")),
            (100, Some("Log !99+")),
            (5_000, Some("Log !99+")),
        ] {
            let row = draw_notice_bar(
                120,
                &StatusState::Idle,
                PermissionMode::Normal,
                None,
                LogAwareness::Hidden,
                unseen,
            );
            match expected {
                Some(segment) => {
                    assert_eq!(row.matches("Log !").count(), 1, "{unseen}: {row}");
                    assert!(row.trim_end().ends_with(segment), "{unseen}: {row}");
                }
                None => assert!(!row.contains("Log !"), "{unseen}: {row}"),
            }
        }
    }

    #[test]
    fn story21_1_notice_badge_sits_beside_the_log_reminder_and_yields_before_the_hint() {
        // The `log: N` reminder keeps the right edge; the badge is admitted
        // beside it and the hint, and is the first thing dropped when the
        // hint alone fits but the hint plus the badge would not.
        let row = draw_notice_bar(
            120,
            &StatusState::Idle,
            PermissionMode::Normal,
            Some("^X d detach"),
            LogAwareness::Unseen(7),
            3,
        );
        assert!(row.trim_end().ends_with(" log: 7"), "{row}");
        assert!(row.contains("Log !3 ^X d detach"), "{row}");

        // 80 columns: the left side is ~23 cells, a 54-cell hint fits alone
        // (78) but not beside the 6-cell badge (85).
        let hint = "h".repeat(54);
        let row = draw_notice_bar(
            80,
            &StatusState::Idle,
            PermissionMode::Normal,
            Some(&hint),
            LogAwareness::Hidden,
            3,
        );
        assert!(row.trim_end().ends_with(&hint), "hint stays: {row}");
        assert!(!row.contains("Log !"), "badge yields first: {row}");
    }
}
