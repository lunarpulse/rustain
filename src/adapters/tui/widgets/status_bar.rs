use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::state::DailyBudgetState;
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
) {
    let status_text = status.display_text();
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
        let pct = used.saturating_mul(100) / context_window;
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
        let truncated = if name.len() > 24 {
            let mut end = 24;
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &name[..end])
        } else {
            name.to_string()
        };
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

    // Contextual hint: right-aligned in remaining space (UX-DR93, UX-DR96)
    // The hint is the first thing truncated if the terminal is too narrow.
    if let Some(hint) = current_hint {
        let left_line = Line::from(left_spans.clone());
        let left_width = left_line
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>();
        let bar_width = area.width as usize;
        let hint_width = hint.chars().count();
        // Only render hint if it fits in the remaining space (with at least 1 gap)
        if left_width + 1 + hint_width <= bar_width {
            let padding = bar_width - left_width - hint_width;
            let mut spans_with_hint = left_spans;
            spans_with_hint.push(Span::raw(" ".repeat(padding)));
            spans_with_hint.push(Span::styled(
                hint,
                theme.typography.hint.fg(theme.colors.text_hint),
            ));
            let line = Line::from(spans_with_hint);
            let widget = Paragraph::new(line).style(Style::default().bg(theme.colors.status_bg));
            return frame.render_widget(widget, area);
        }
    }

    let line = Line::from(left_spans);
    let widget = Paragraph::new(line).style(Style::default().bg(theme.colors.status_bg));
    frame.render_widget(widget, area);
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
        assert_eq!(
            StatusState::Flash {
                message: "Config error".to_string(),
                remaining_ms: 1000,
            }
            .display_text(),
            "Config error"
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
        assert!(
            !StatusState::Flash {
                message: "test".to_string(),
                remaining_ms: 1000,
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
            );
        })
        .unwrap();
        let txt = buffer_text(&t);
        assert!(
            txt.contains("+123 ctx"),
            "expected `+123 ctx` segment: {txt}"
        );
    }
}
