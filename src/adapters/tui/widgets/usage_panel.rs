//! Usage / cost panel overlay (Ctrl+X, U). Story 7.5 AC3 + AC4 (UX-DR111/113).
//!
//! Renders 4 sections in a single bordered modal:
//!   1. Turn breakdown (current conversation)
//!   2. Session today (aggregated cost)
//!   3. Context window gauge + cache %
//!   4. Per-model breakdown (only if >1 model used today)
//!
//! Plus a footer line per AC4 — `│ ${X.YZ} · {N} tasks · {Tm Ts} │`.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::state::{SessionUsageSummary, UsagePanelState};
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::model_selector::humanize_ctx;
use crate::adapters::tui::widgets::status_bar::format_token_count;

/// USD formatter: 2-decimal for ≥$0.01, 4-decimal for <$0.01 (Story 7.5 Dev Notes §"Cost formatting").
pub(crate) fn format_cost(usd: f64) -> String {
    let usd = usd.max(0.0);
    if usd >= 0.01 {
        format!("${:.2}", usd)
    } else {
        format!("${:.4}", usd)
    }
}

/// `cost_or_na`: `Some(c)` → `${X.YZ}`; `None` → `"n/a"` (AC6).
pub(crate) fn cost_or_na(usd: Option<f64>) -> String {
    match usd {
        Some(c) => format_cost(c),
        None => "n/a".to_string(),
    }
}

/// `{minutes}m {seconds}s` for the AC4 footer.
fn format_elapsed(secs: i64) -> String {
    let s = secs.max(0);
    format!("{}m {}s", s / 60, s % 60)
}

pub fn render(frame: &mut Frame, area: Rect, state: &UsagePanelState, theme: &Theme) {
    if !state.active {
        return;
    }

    let modal_area = calculate_centered_area(area);
    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(Span::styled(" Usage & Cost ", theme.typography.heading))
        .title_bottom(Span::styled(
            " Esc: close ",
            Style::default().fg(theme.colors.fg_muted),
        ));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let mut lines: Vec<Line> = Vec::new();

    // ── Section 1 — Turn breakdown ────────────────────────────────────
    lines.push(Line::from(Span::styled(
        "Turn breakdown",
        Style::default()
            .fg(theme.colors.fg_secondary)
            .add_modifier(Modifier::BOLD),
    )));
    if state.turn_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no turns yet)",
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        for row in state.turn_rows.iter().take(8) {
            let model_trunc = truncate_model(&row.model, 24);
            lines.push(Line::from(format!(
                "  [t-{}] {} · ↑{} ↓{} · {}",
                row.turn_index,
                model_trunc,
                format_token_count(row.tokens_in),
                format_token_count(row.tokens_out),
                cost_or_na(row.cost_usd),
            )));
        }
    }

    section_rule(&mut lines, inner.width, theme);

    // ── Section 2 — Today ──────────────────────────────────────────────
    lines.push(Line::from(Span::styled(
        "Today",
        Style::default()
            .fg(theme.colors.fg_secondary)
            .add_modifier(Modifier::BOLD),
    )));
    let today = &state.session_today;
    lines.push(Line::from(format!(
        "  ↑{} ↓{}",
        format_token_count((today.tokens_in.min(u64::from(u32::MAX))) as u32),
        format_token_count((today.tokens_out.min(u64::from(u32::MAX))) as u32),
    )));
    lines.push(Line::from(format!(
        "  total: {}",
        cost_or_na(today.cost_usd)
    )));

    section_rule(&mut lines, inner.width, theme);

    // ── Section 3 — Context window + cache ─────────────────────────────
    lines.push(Line::from(Span::styled(
        "Context window",
        Style::default()
            .fg(theme.colors.fg_secondary)
            .add_modifier(Modifier::BOLD),
    )));
    if state.context_window_tokens > 0 {
        let used = state.context_used_tokens;
        let pct = used.saturating_mul(100) / state.context_window_tokens.max(1);
        let color = if pct >= 95 {
            theme.colors.error
        } else if pct >= 80 {
            theme.colors.warning
        } else {
            theme.colors.status_fg
        };
        lines.push(Line::from(Span::styled(
            format!(
                "  ctx: {}/{} ({}%)",
                humanize_ctx(used),
                humanize_ctx(state.context_window_tokens),
                pct
            ),
            Style::default().fg(color),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  ctx: n/a",
            Style::default().fg(theme.colors.fg_muted),
        )));
    }
    if today.cache_read_tokens > 0 {
        let hit_pct = if today.cache_total_tokens > 0 {
            (today.cache_read_tokens.saturating_mul(100) / today.cache_total_tokens.max(1)).min(100)
        } else {
            0
        };
        lines.push(Line::from(format!(
            "  cache: hit {}% · saved {}",
            hit_pct,
            format_cost(today.cache_savings_usd)
        )));
    }

    // ── Section 4 — Per-model breakdown (only when >1 model) ───────────
    if state.per_model.len() > 1 {
        section_rule(&mut lines, inner.width, theme);
        lines.push(Line::from(Span::styled(
            "Per-model",
            Style::default()
                .fg(theme.colors.fg_secondary)
                .add_modifier(Modifier::BOLD),
        )));
        // Sort by cost desc
        let mut rows: Vec<(
            &String,
            &crate::domain::services::cost_calculator::ModelCost,
        )> = state.per_model.iter().collect();
        rows.sort_by(|a, b| {
            b.1.cost_usd
                .unwrap_or(0.0)
                .partial_cmp(&a.1.cost_usd.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let total_cost = today.cost_usd.unwrap_or(0.0);
        for (model, mc) in rows {
            let pct = if total_cost > 0.0 {
                ((mc.cost_usd.unwrap_or(0.0) / total_cost) * 100.0).round() as u32
            } else {
                0
            };
            lines.push(Line::from(format!(
                "  {} · ↑{} ↓{} · {} ({}%)",
                truncate_model(model, 24),
                format_token_count((mc.tokens_in.min(u64::from(u32::MAX))) as u32),
                format_token_count((mc.tokens_out.min(u64::from(u32::MAX))) as u32),
                cost_or_na(mc.cost_usd),
                pct,
            )));
        }
    }

    // AC6 — note missing pricing models in panel footer area.
    if !state.missing_pricing_models.is_empty() {
        section_rule(&mut lines, inner.width, theme);
        for m in &state.missing_pricing_models {
            lines.push(Line::from(Span::styled(
                format!("  (no pricing configured for {})", truncate_model(m, 30)),
                Style::default().fg(theme.colors.fg_muted),
            )));
        }
    }

    // ── Footer (AC4) ───────────────────────────────────────────────────
    section_rule(&mut lines, inner.width, theme);
    lines.push(footer_line(today, theme));

    let content = Paragraph::new(lines).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_surface),
    );
    frame.render_widget(content, inner);
}

fn footer_line<'a>(today: &SessionUsageSummary, theme: &Theme) -> Line<'a> {
    let cost_part = match today.cost_usd {
        Some(c) => format_cost(c),
        None => "cost: n/a".to_string(),
    };
    Line::from(Span::styled(
        format!(
            "│ {} · {} tasks · {} │",
            cost_part,
            today.task_count,
            format_elapsed(today.elapsed_secs)
        ),
        Style::default().fg(theme.colors.fg_secondary),
    ))
}

fn section_rule(lines: &mut Vec<Line<'static>>, width: u16, theme: &Theme) {
    lines.push(Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(theme.colors.fg_muted),
    )));
}

fn truncate_model(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn calculate_centered_area(area: Rect) -> Rect {
    let width = (area.width * 70 / 100).clamp(50, 80).min(area.width);
    let height = (area.height * 70 / 100).clamp(12, 24).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::state::{TurnUsageRow, UsagePanelState};
    use crate::domain::services::cost_calculator::ModelCost;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn theme() -> Theme {
        Theme::dark()
    }

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
    fn renders_inactive_panel_as_noop() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let state = UsagePanelState::new();
        term.draw(|frame| {
            render(frame, frame.area(), &state, &theme());
        })
        .unwrap();
        // Inactive — no title rendered
        let txt = buffer_text(&term);
        assert!(
            !txt.contains("Usage & Cost"),
            "inactive panel shouldn't render title"
        );
    }

    #[test]
    fn renders_active_panel_with_title_and_no_turns_message() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = UsagePanelState::new();
        state.active = true;
        term.draw(|frame| {
            render(frame, frame.area(), &state, &theme());
        })
        .unwrap();
        let txt = buffer_text(&term);
        assert!(txt.contains("Usage & Cost"), "title missing: {txt}");
        assert!(
            txt.contains("(no turns yet)"),
            "empty-state message missing: {txt}"
        );
    }

    #[test]
    fn renders_n_a_when_pricing_missing() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = UsagePanelState::new();
        state.active = true;
        state.turn_rows.push(TurnUsageRow {
            turn_index: 0,
            model: "unknown-model".to_string(),
            tokens_in: 1200,
            tokens_out: 340,
            cost_usd: None,
        });
        state
            .missing_pricing_models
            .push("unknown-model".to_string());
        term.draw(|frame| {
            render(frame, frame.area(), &state, &theme());
        })
        .unwrap();
        let txt = buffer_text(&term);
        assert!(txt.contains("n/a"), "n/a missing: {txt}");
        assert!(
            txt.contains("(no pricing configured for unknown-model)"),
            "missing-pricing footer line not rendered: {txt}"
        );
    }

    #[test]
    fn footer_renders_cost_tasks_elapsed() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = UsagePanelState::new();
        state.active = true;
        state.session_today = SessionUsageSummary {
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: Some(0.12),
            task_count: 5,
            elapsed_secs: 134, // 2m 14s
            cache_read_tokens: 0,
            cache_total_tokens: 0,
            cache_savings_usd: 0.0,
        };
        term.draw(|frame| {
            render(frame, frame.area(), &state, &theme());
        })
        .unwrap();
        let txt = buffer_text(&term);
        assert!(txt.contains("$0.12"), "cost not formatted: {txt}");
        assert!(txt.contains("5 tasks"), "task count missing: {txt}");
        assert!(txt.contains("2m 14s"), "elapsed missing: {txt}");
    }

    #[test]
    fn per_model_breakdown_only_when_more_than_one_model() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = UsagePanelState::new();
        state.active = true;
        let mut pm: BTreeMap<String, ModelCost> = BTreeMap::new();
        pm.insert(
            "sonnet".to_string(),
            ModelCost {
                tokens_in: 1000,
                tokens_out: 500,
                cost_usd: Some(0.10),
                call_count: 1,
            },
        );
        // Single model — section should NOT render
        state.per_model = pm.clone();
        term.draw(|frame| render(frame, frame.area(), &state, &theme()))
            .unwrap();
        let txt = buffer_text(&term);
        assert!(
            !txt.contains("Per-model"),
            "Per-model section should be omitted for single model: {txt}"
        );

        // Two models — section should render
        pm.insert(
            "haiku".to_string(),
            ModelCost {
                tokens_in: 100,
                tokens_out: 50,
                cost_usd: Some(0.01),
                call_count: 1,
            },
        );
        state.per_model = pm;
        state.session_today.cost_usd = Some(0.11);
        let mut term2 = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term2
            .draw(|frame| render(frame, frame.area(), &state, &theme()))
            .unwrap();
        let txt2 = buffer_text(&term2);
        assert!(
            txt2.contains("Per-model"),
            "Per-model section should render for >1 models: {txt2}"
        );
    }
}
