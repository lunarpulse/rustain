//! Adapter Status panel widget — Story 8.5 AC-2, AC-3.
//!
//! Renders a 7-row list of port dimensions with health indicators,
//! adapter names, override markers, and key metrics. Pull-based
//! per-render-tick: calls `health_snapshot()` on each loaded adapter.

use std::collections::BTreeMap;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::{AdapterRef, HealthLevel, HealthSummary, McpHealthRow, PortDimension};
use crate::domain::services::adapter_overlay;
use crate::infrastructure::runtime::agent_core::AgentCore;

const PORT_COUNT: usize = 7;

const PORTS: [(PortDimension, &str); PORT_COUNT] = [
    (PortDimension::Persona, "persona"),
    (PortDimension::Memory, "memory"),
    (PortDimension::Session, "session"),
    (PortDimension::Tools, "tools"),
    (PortDimension::Channels, "channels"),
    (PortDimension::Scheduler, "scheduler"),
    (PortDimension::Context, "context"),
];

pub fn port_count() -> usize {
    PORT_COUNT
}

fn health_color(level: HealthLevel, theme: &Theme) -> Color {
    match level {
        HealthLevel::Healthy => theme.colors.success,
        HealthLevel::Degraded => theme.colors.warning,
        HealthLevel::Error => theme.colors.error,
        HealthLevel::Unknown => theme.colors.fg_muted,
    }
}

/// One MCP sub-row: `└─ <glyph> <server> <transport>` with the metric
/// right-aligned by display cells.
fn mcp_health_line(row: &McpHealthRow, inner_width: u16, theme: &Theme) -> Line<'static> {
    let sym = row.level.symbol();
    let sym_color = health_color(row.level, theme);
    let mut spans = vec![Span::styled(
        format!("   └─ {} {} {}", sym, row.server_name, row.transport),
        Style::default().fg(sym_color),
    )];
    let used_width: usize = spans.iter().map(|s| geom::cells(&s.content)).sum();
    let available = inner_width.saturating_sub(3) as usize;
    if available > used_width + 2 {
        let pad = available.saturating_sub(used_width + geom::cells(&row.metric));
        spans.push(Span::raw(geom::spaces(pad)));
    }
    spans.push(Span::styled(
        row.metric.clone(),
        Style::default().fg(theme.colors.fg_muted),
    ));
    Line::from(spans)
}

fn get_adapter_name_from_core(agent_core: &AgentCore, port: PortDimension) -> String {
    let health = get_health_summary(agent_core, port);
    if health.metric != "n/a" {
        health.metric.clone()
    } else {
        let label = adapter_overlay::port_label(port);
        label.to_string()
    }
}

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    agent_core: &AgentCore,
    overrides: &BTreeMap<PortDimension, AdapterRef>,
    focused: bool,
    selected_row: usize,
    theme: &Theme,
) {
    let border_color = if focused {
        theme.colors.accent
    } else {
        theme.colors.fg_secondary
    };

    let block = Block::default()
        .title(" Adapters ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width < 10 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mcp_rows = get_mcp_health_rows(agent_core);

    for (i, (port, label)) in PORTS.iter().enumerate() {
        let health = get_health_summary(agent_core, *port);
        let symbol = health.level.symbol();
        let sym_color = health_color(health.level, theme);

        let core_name = get_adapter_name_from_core(agent_core, *port);
        let adapter_name = adapter_overlay::active_adapter_for(*port, &core_name, overrides);

        let has_override = overrides.contains_key(port);
        let mut spans = Vec::new();

        spans.push(Span::styled(
            format!("{} ", symbol),
            Style::default().fg(sym_color),
        ));

        spans.push(Span::styled(
            format!("{:12}", label),
            Style::default().fg(theme.colors.fg_primary),
        ));

        let name_width = 24usize.min(inner.width.saturating_sub(40) as usize);
        let truncated = geom::ellipsize(&adapter_name, name_width, "…");
        spans.push(Span::styled(
            format!(": {}", truncated),
            Style::default().fg(theme.colors.fg_primary),
        ));
        if has_override {
            spans.push(Span::styled(
                " [override]",
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let metric = health.metric.clone();
        let action_opt = health.suggested_action;
        let level = health.level;
        let used_width: usize = spans.iter().map(|s| geom::cells(&s.content)).sum();
        let available = inner.width.saturating_sub(3) as usize;
        if available > used_width + 2 {
            let pad = available.saturating_sub(used_width + geom::cells(&metric));
            spans.push(Span::raw(geom::spaces(pad)));
        }

        let metric_color = match level {
            HealthLevel::Degraded => theme.colors.warning,
            HealthLevel::Error => theme.colors.error,
            _ => theme.colors.fg_muted,
        };
        spans.push(Span::styled(metric, Style::default().fg(metric_color)));

        if i == selected_row {
            lines.push(Line::from(spans).style(Style::default().add_modifier(Modifier::REVERSED)));
        } else {
            lines.push(Line::from(spans));
        }

        if let Some(action) = action_opt {
            if level == HealthLevel::Degraded || level == HealthLevel::Error {
                lines.push(Line::from(Span::styled(
                    format!("  → {}", action),
                    Style::default().fg(theme.colors.fg_muted),
                )));
            }
        }

        // Insert MCP sub-rows after the tools row (Story 9.1 AC-5)
        if *port == PortDimension::Tools && !mcp_rows.is_empty() {
            for row in &mcp_rows {
                lines.push(mcp_health_line(row, inner.width, theme));
            }
            // Story 9.3a — registry summary line
            if let Some(summary) = get_registry_summary(agent_core) {
                lines.push(Line::from(Span::styled(
                    format!("   └─ {}", summary),
                    Style::default().fg(theme.colors.fg_muted),
                )));
            }
        }
    }

    let all_noop = overrides.is_empty();
    if all_noop {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled(
            "7 ports loaded — most adapters NoOp (real adapters in Epic 12)",
            Style::default().fg(theme.colors.fg_muted),
        )));
    }

    let max_lines = inner.height as usize;
    for (i, line) in lines.iter().enumerate().take(max_lines) {
        let y = inner.y + i as u16;
        if y < area.bottom() {
            buf.set_line(inner.x, y, line, inner.width);
        }
    }
}

fn get_health_summary(agent_core: &AgentCore, port: PortDimension) -> HealthSummary {
    match port {
        PortDimension::Persona => agent_core.persona.load_full().health_snapshot(),
        PortDimension::Memory => agent_core.memory.load_full().health_snapshot(),
        PortDimension::Session => agent_core.session.load_full().health_snapshot(),
        PortDimension::Tools => agent_core.tools.load_full().health_snapshot(),
        PortDimension::Channels => agent_core.channels.load_full().health_snapshot(),
        PortDimension::Scheduler => agent_core.scheduler.load_full().health_snapshot(),
        PortDimension::Context => agent_core.context.load_full().health_snapshot(),
        PortDimension::Skills => crate::domain::models::HealthSummary::unknown(),
    }
}

#[cfg(feature = "mcp")]
fn get_mcp_health_rows(agent_core: &AgentCore) -> Vec<McpHealthRow> {
    use crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    let tools = agent_core.tools.load_full();
    if let Some(composite) = tools.as_any().downcast_ref::<CompositeToolsetAdapter>() {
        composite.mcp_health_rows()
    } else {
        Vec::new()
    }
}

#[cfg(not(feature = "mcp"))]
fn get_mcp_health_rows(_agent_core: &AgentCore) -> Vec<McpHealthRow> {
    Vec::new()
}

/// Story 9.3a — format a registry summary from a snapshot of capabilities.
/// Returns `None` when the snapshot is empty.
///
/// Extracted as a pure function so tests can verify the formatting
/// without constructing an `AgentCore`.
///
/// Story 9.3b — extended to show all three protocols (MCP, builtin, skill).
pub fn format_registry_summary(
    snap: &[crate::domain::models::capability_registry::RegisteredCapability],
) -> Option<String> {
    if snap.is_empty() {
        return None;
    }
    let mcp_count = snap.iter().filter(|c| c.protocol == "mcp").count();
    let builtin_count = snap.iter().filter(|c| c.protocol == "builtin").count();
    let skill_count = snap.iter().filter(|c| c.protocol == "skill").count();
    Some(format!(
        "Registry: {} capabilities ({} MCP, {} builtin, {} skill)",
        snap.len(),
        mcp_count,
        builtin_count,
        skill_count
    ))
}

/// Story 9.3a — registry summary line for the adapter status panel.
/// Returns `None` when the registry is empty (no registered capabilities).
#[cfg(feature = "mcp")]
fn get_registry_summary(agent_core: &AgentCore) -> Option<String> {
    use crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    let tools = agent_core.tools.load_full();
    if let Some(composite) = tools.as_any().downcast_ref::<CompositeToolsetAdapter>() {
        let snap = composite.capability_registry().snapshot();
        format_registry_summary(&snap)
    } else {
        None
    }
}

#[cfg(not(feature = "mcp"))]
fn get_registry_summary(_agent_core: &AgentCore) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;

    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const C: &str = "界";
    const A: &str = "e\u{301}";

    fn glyphs(buf: &Buffer, y: u16) -> Vec<(u16, String)> {
        let mut out = Vec::new();
        let mut x = buf.area.left();
        while x < buf.area.right() {
            let symbol = buf
                .cell((x, y))
                .map(|c| c.symbol().to_string())
                .unwrap_or_default();
            let step = u16::try_from(geom::cells(&symbol).max(1)).unwrap_or(1);
            out.push((x, symbol));
            x = x.saturating_add(step);
        }
        out
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        let joined: String = glyphs(buf, y).into_iter().map(|(_, s)| s).collect();
        joined.trim_end().to_string()
    }

    /// Right edge (exclusive) of the last non-blank glyph left of `limit`.
    fn end_x(buf: &Buffer, y: u16, limit: u16) -> u16 {
        glyphs(buf, y)
            .into_iter()
            .filter(|(x, s)| *x < limit && !s.trim().is_empty())
            .map(|(x, s)| x + u16::try_from(geom::cells(&s).max(1)).unwrap_or(1))
            .max()
            .unwrap_or(0)
    }

    fn assert_whole_clusters(text: &str) {
        for g in text.graphemes(true) {
            if g.contains('\u{200d}') || g.contains('👨') {
                assert_eq!(g, G, "partial family cluster in {text:?}");
            }
        }
    }

    #[test]
    fn display_width_adapter_mcp_row_metric_right_aligned_by_cells() {
        let theme = Theme::dark();
        let servers = ["srv".to_string(), format!("{C}{G}"), format!("{G}{A}")];
        let metrics = ["e ready".to_string(), format!("{A} ready")];
        for inner_width in [58u16, 78, 128] {
            for server in &servers {
                for metric in &metrics {
                    let row = McpHealthRow {
                        server_name: server.clone(),
                        transport: "stdio".to_string(),
                        level: HealthLevel::Healthy,
                        metric: metric.clone(),
                    };
                    let line = mcp_health_line(&row, inner_width, &theme);
                    let mut buf = Buffer::empty(Rect::new(0, 0, inner_width, 1));
                    buf.set_line(0, 0, &line, inner_width);
                    assert_eq!(
                        end_x(&buf, 0, inner_width),
                        inner_width - 3,
                        "server {server:?} metric {metric:?}: {:?}",
                        row_text(&buf, 0)
                    );
                    let text = row_text(&buf, 0);
                    assert!(text.contains(server.as_str()), "{text:?}");
                    assert_whole_clusters(&text);
                }
            }
        }
    }
}
