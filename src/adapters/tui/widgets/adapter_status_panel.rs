//! Adapter status rows — Story 8.5 AC-2, AC-3; the body of the Adapters window (Story 21.7 A21).
//!
//! A snapshot of the 7 port dimensions with health indicators, adapter names, override markers
//! and key metrics ([`snapshot`]: one `health_snapshot()` per loaded adapter, taken when the
//! window opens), and the line builder that lays the snapshot out at a width ([`lines`]). The
//! snapshot is plain data so the window can scroll it, and so the pure builder ([`build`]) can
//! be proven without an `AgentCore`.

use std::collections::BTreeMap;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::{AdapterRef, HealthLevel, HealthSummary, McpHealthRow, PortDimension};
use crate::domain::services::adapter_overlay;
use crate::domain::services::peer_text::sanitize_display_line;
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

/// One logical row of the Adapters window.
#[derive(Debug, Clone)]
pub enum AdapterLine {
    /// A port dimension: its health, its active adapter, whether the session overrides it, its
    /// metric and (when degraded or failing) the suggested action.
    Port {
        level: HealthLevel,
        label: &'static str,
        adapter: String,
        overridden: bool,
        metric: String,
        action: Option<&'static str>,
    },
    /// An MCP server under the tools port (Story 9.1 AC-5).
    Mcp(McpHealthRow),
    /// The capability registry summary (Story 9.3a).
    Registry(String),
    Blank,
    /// A muted note (the NoOp footer).
    Note(String),
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

/// The rows of the window from the health of every port, the MCP rows, the registry summary and
/// the session's overrides. Pure: the rows are what `lines` lays out.
#[must_use]
pub fn build(
    health: impl Fn(PortDimension) -> HealthSummary,
    mcp_rows: Vec<McpHealthRow>,
    registry_summary: Option<String>,
    overrides: &BTreeMap<PortDimension, AdapterRef>,
) -> Vec<AdapterLine> {
    let mut rows = Vec::with_capacity(PORT_COUNT + mcp_rows.len() + 3);
    for (port, label) in PORTS {
        let summary = health(port);
        // The adapter's own name is its metric when it has one; a NoOp port reports `n/a` and
        // shows the port's label.
        let core_name = if summary.metric == "n/a" {
            adapter_overlay::port_label(port).to_string()
        } else {
            summary.metric.clone()
        };
        let action = summary
            .suggested_action
            .filter(|_| matches!(summary.level, HealthLevel::Degraded | HealthLevel::Error));
        rows.push(AdapterLine::Port {
            level: summary.level,
            label,
            adapter: sanitize_display_line(&adapter_overlay::active_adapter_for(
                port, &core_name, overrides,
            )),
            overridden: overrides.contains_key(&port),
            metric: sanitize_display_line(&summary.metric),
            action,
        });
        // MCP sub-rows after the tools row (Story 9.1 AC-5), then the registry summary.
        if port == PortDimension::Tools && !mcp_rows.is_empty() {
            // Server names, transports and metrics come from MCP configuration and servers:
            // each scalar is sanitized here, before any span is measured or built (A9).
            rows.extend(mcp_rows.iter().map(|row| {
                AdapterLine::Mcp(McpHealthRow {
                    server_name: sanitize_display_line(&row.server_name),
                    transport: sanitize_display_line(&row.transport),
                    level: row.level,
                    metric: sanitize_display_line(&row.metric),
                })
            }));
            if let Some(summary) = &registry_summary {
                rows.push(AdapterLine::Registry(sanitize_display_line(summary)));
            }
        }
    }
    if overrides.is_empty() {
        rows.push(AdapterLine::Blank);
        rows.push(AdapterLine::Note(format!(
            "{PORT_COUNT} adapters loaded; most are placeholders."
        )));
    }
    rows
}

/// The snapshot the window opens with: every loaded adapter's `health_snapshot()` once.
#[must_use]
pub fn snapshot(
    agent_core: &AgentCore,
    overrides: &BTreeMap<PortDimension, AdapterRef>,
) -> Vec<AdapterLine> {
    build(
        |port| get_health_summary(agent_core, port),
        get_mcp_health_rows(agent_core),
        get_registry_summary(agent_core),
        overrides,
    )
}

/// The rows at `inner_width` cells, one painted line each (a degraded port adds its action
/// line). Height is `lines(..).len()`: the window clamps its scroll against exactly this.
#[must_use]
pub fn lines(rows: &[AdapterLine], inner_width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(rows.len() + 2);
    for row in rows {
        match row {
            AdapterLine::Port {
                level,
                label,
                adapter,
                overridden,
                metric,
                action,
            } => {
                let mut spans = vec![
                    Span::styled(
                        format!("{} ", level.symbol()),
                        Style::default().fg(health_color(*level, theme)),
                    ),
                    Span::styled(
                        format!("{label:12}"),
                        Style::default().fg(theme.colors.fg_primary),
                    ),
                ];
                let name_width = 24usize.min(inner_width.saturating_sub(40) as usize);
                let truncated = geom::ellipsize(adapter, name_width, "…");
                spans.push(Span::styled(
                    format!(": {truncated}"),
                    Style::default().fg(theme.colors.fg_primary),
                ));
                if *overridden {
                    spans.push(Span::styled(
                        " [override]",
                        Style::default()
                            .fg(theme.colors.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                let used_width: usize = spans.iter().map(|s| geom::cells(&s.content)).sum();
                let available = inner_width.saturating_sub(3) as usize;
                if available > used_width + 2 {
                    let pad = available.saturating_sub(used_width + geom::cells(metric));
                    spans.push(Span::raw(geom::spaces(pad)));
                }
                let metric_color = match level {
                    HealthLevel::Degraded => theme.colors.warning,
                    HealthLevel::Error => theme.colors.error,
                    _ => theme.colors.fg_muted,
                };
                spans.push(Span::styled(
                    metric.clone(),
                    Style::default().fg(metric_color),
                ));
                out.push(Line::from(spans));
                if let Some(action) = action {
                    out.push(Line::from(Span::styled(
                        format!("  → {action}"),
                        Style::default().fg(theme.colors.fg_muted),
                    )));
                }
            }
            AdapterLine::Mcp(row) => out.push(mcp_health_line(row, inner_width, theme)),
            AdapterLine::Registry(summary) => out.push(Line::from(Span::styled(
                format!("   └─ {summary}"),
                Style::default().fg(theme.colors.fg_muted),
            ))),
            AdapterLine::Blank => out.push(Line::from(String::new())),
            AdapterLine::Note(note) => out.push(Line::from(Span::styled(
                note.clone(),
                Style::default().fg(theme.colors.fg_muted),
            ))),
        }
    }
    out
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
    use ratatui::{buffer::Buffer, layout::Rect};
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

    fn healthy(metric: &str) -> HealthSummary {
        HealthSummary::healthy(metric.to_owned())
    }

    fn painted(rows: &[AdapterLine], width: u16) -> Vec<String> {
        lines(rows, width, &Theme::dark())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn story21_7_the_rows_are_the_seven_ports_with_mcp_rows_under_tools_and_the_noop_footer() {
        let mcp = McpHealthRow {
            server_name: "echo".to_owned(),
            transport: "stdio".to_owned(),
            level: HealthLevel::Healthy,
            metric: "7 tools".to_owned(),
        };
        let rows = build(
            |_| HealthSummary::unknown(),
            vec![mcp],
            Some("Registry: 9 capabilities (7 MCP, 2 builtin, 0 skill)".to_owned()),
            &BTreeMap::new(),
        );
        let text = painted(&rows, 78);
        let labels = [
            "persona",
            "memory",
            "session",
            "tools",
            "channels",
            "scheduler",
            "context",
        ];
        for (row, label) in text.iter().zip(labels.iter().take(4)) {
            assert!(row.contains(label), "{row:?}");
        }
        assert!(text[4].starts_with("   └─ "), "{:?}", text[4]);
        assert!(text[4].contains("echo stdio"), "{:?}", text[4]);
        assert!(text[4].ends_with("7 tools"), "{:?}", text[4]);
        assert!(
            text[5].contains("Registry: 9 capabilities"),
            "{:?}",
            text[5]
        );
        assert!(text[6].contains("channels"), "{:?}", text[6]);
        assert_eq!(text[text.len() - 2], "");
        assert!(text.last().unwrap().contains("7 adapters loaded"));
    }

    #[test]
    fn story21_7_an_override_is_marked_and_names_its_adapter_and_degraded_ports_add_their_action() {
        let overrides = BTreeMap::from([(
            PortDimension::Scheduler,
            AdapterRef {
                adapter: "cron".to_owned(),
                _config: None,
            },
        )]);
        let rows = build(
            |port| {
                if port == PortDimension::Memory {
                    HealthSummary {
                        level: HealthLevel::Degraded,
                        metric: "slow".to_owned(),
                        suggested_action: Some("restart the store"),
                    }
                } else {
                    healthy("ok")
                }
            },
            Vec::new(),
            None,
            &overrides,
        );
        let text = painted(&rows, 78);
        assert!(
            text.iter()
                .any(|row| row.contains("scheduler") && row.contains(": cron [override]")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|row| row == "  → restart the store"),
            "{text:?}"
        );
        assert!(
            !text.iter().any(|row| row.contains("7 adapters loaded")),
            "overrides drop the footer"
        );
    }

    #[test]
    fn story21_14_the_footer_says_how_many_adapters_loaded_and_that_most_are_placeholders() {
        let rows = build(|_| healthy("ok"), Vec::new(), None, &BTreeMap::new());
        let text = painted(&rows, 78);
        assert_eq!(
            text.last().map(String::as_str),
            Some("7 adapters loaded; most are placeholders.")
        );
    }
}
