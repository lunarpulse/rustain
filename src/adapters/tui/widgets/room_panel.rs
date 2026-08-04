//! Durable room viewer panel (`Ctrl+X, R` / `/room`). Story 18.3a, AC1 / AC2.
//!
//! Renders the [`NodeView`]s produced by
//! [`crate::domain::models::OrchestrationRoom::project_for_host`] — the
//! host-honest fold of the one journal. Never `project()`: the host-bound
//! derivation is what makes AC2 possible.
//!
//! # Honesty rules this widget enforces
//!
//! - **Never claims to be live.** The header says "as of <time>", because
//!   `NodeJournal::load()` under a shared `flock` is a consistent read, not a
//!   subscription: the daemon can append between two reads.
//! - **Never moves the viewport under a reader.** New entries are counted into
//!   a "N newer entries" affordance at the boundary instead.
//! - **Never claims tamper-evidence.** The footer states the real guarantee,
//!   bound to [`STRUCTURAL_REPLAY_CLAIM`]. No string here may describe the
//!   journal or the room as authentic, tamper-evident, or cryptographically
//!   verifiable (`DF-18-2-AUTHENTICATED-JOURNAL`).
//! - **Monochrome-safe.** Every glyph is paired with a word.
//! - **Three distinguishable zero-states.** A blank pane cannot tell "not
//!   attached" from "no nodes" from "all terminal"; each says which it is.
//!
//! # One glyph set, not two
//!
//! Rows reuse [`super::orchestration_glyph`] — `ownership_glyph`,
//! `node_state_glyph`, `isolation_glyph`. This module mints no glyph.
//!
//! `▲` here means **attention required**, present on the same frame the
//! host-bound flag is true. The shipped `▲` in the agent panel is dwell-based
//! (60 s). That is not a second vocabulary: dwell is one producer of "attention
//! required", host-bound is another. The glyph's producer set widens; the glyph
//! does not fork.
//!
//! # What is deliberately NOT rendered
//!
//! The UX spec's wave aggregate reads
//! `wave ✓9 ✗4 ▶2 · $0.40/$2.00 · [cancel all]`. This panel renders the
//! `wave ✓N ✗N ▶N` term only:
//!
//! - **The cost term is omitted, not zeroed.** `OrchestrationRoom` folds no
//!   cost, and a rendered `$0.00/$0.00` would be a fabricated number in a
//!   surface whose entire purpose is honesty.
//! - **`[cancel all]` is omitted.** `/room` is a read-only replay, and an
//!   action label that does not dispatch is the exact defect class 18.3c
//!   shipped (`[✗] Retract` as inert text). A cancel affordance needs a real
//!   cancel path for durable-room waves; that is not this story's.
//!
//! Both are deferred capability (Rule 3), not defects: no invariant fails, and
//! the room simply does not know those facts yet.

use ratatui::prelude::*;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::adapters::tui::state::{RoomPanelState, RoomZeroState};
use crate::adapters::tui::theme::Theme;
use crate::domain::models::{NodeState, NodeView, OrchestrationRoom};
use crate::domain::services::transparency::{STRUCTURAL_REPLAY_CLAIM, format_unix_millis};

use super::orchestration_glyph::{isolation_glyph, node_state_glyph};
use super::sidebar::truncate_to_width;

/// Label for an unrecognized durable record. Reuses the exact string the
/// transparency projection already ships, so the two surfaces cannot drift
/// into two spellings of the same admission.
pub const UNKNOWN_RECORD_LABEL: &str = "unrecognised record — written by a newer build";

/// Ownership glyph for a room row.
///
/// `NodeView` carries `NodeOrigin`, not `OwnershipKind`, so the col-1 glyph is
/// derived here from origin: `◆` for work this host owns, `▸` for work that
/// arrived from elsewhere. Both characters come from the row grammar the UX
/// spec already fixed; nothing new is minted.
fn origin_glyph(view: &NodeView) -> &'static str {
    match view.origin {
        crate::domain::models::NodeOrigin::Remote => "\u{25B8}", // ▸
        _ => "\u{25C6}",                                         // ◆
    }
}

/// One node row: `[ownership][state] name   [hazard-zone]   [headroom]`.
///
/// # AC2 — the host-bound suffix REPLACES `resumable`
///
/// On a node whose recorded host binding is not the host this fold was made
/// against, the row reads `· host-bound: <host>  ▲`. The `resumable` suffix is
/// **not** appended alongside it — it is gone. Option (C) of OPEN-DR-1 ("keep
/// `resumable`, add a hazard") was rejected precisely because it leaves the lie
/// on screen with an annotation.
///
/// **Latched.** Nothing here promises resume-on-host-return: no `recovering…`,
/// no spinner, no "will retry". `project_for_host` re-derives the flag on every
/// fold, but no producer ever re-binds `view.host` back to a returning host, so
/// as a UX contract the marker stays until a paired host-reattach story ships
/// the clearing producer (`DF-18-3a-HOSTRETURN`).
///
/// **Truncation order.** The reason is placed before any long identifier: the
/// sidebar is ~35 columns and a panel that renders but tells you nothing is
/// what 18.2's smoke test caught.
#[must_use]
pub fn node_row(view: &NodeView) -> String {
    let mut row = format!(
        "{}{} {}",
        origin_glyph(view),
        node_state_glyph(view.state),
        view.id.as_str()
    );
    if view.host_bound_unavailable {
        // REPLACES the `resumable` suffix. Names the host, so the operator
        // knows *which* machine the work is stranded on (NFR70(d)).
        row.push_str(&format!(
            " \u{b7} host-bound: {}  \u{25b2}",
            view.host.host_id
        ));
    } else if view.state == NodeState::Suspended {
        row.push_str(" \u{b7} resumable");
    }
    if !view.open_tickets.is_empty() {
        row.push_str(&format!(" \u{b7} {} open", view.open_tickets.len()));
    }
    if view.mcp_task.is_some() {
        row.push_str(&format!(" \u{b7} {}", isolation_glyph()));
    }
    row
}

fn visible_node_row(view: &NodeView, width: usize) -> String {
    let canonical = node_row(view);
    let truncated = truncate_to_width(&canonical, width);
    let full_warning = format!("host-bound: {}", view.host.host_id);
    if !view.host_bound_unavailable
        || (truncated.contains(&full_warning) && truncated.contains('\u{25b2}'))
    {
        return truncated;
    }
    // Narrow-layout fallback: safety reason and hazard precede both unbounded
    // identifiers. Wide layouts retain the canonical AC2 row grammar.
    truncate_to_width(
        &format!(
            "{}{} \u{b7} host-bound: {} \u{25b2} {}",
            origin_glyph(view),
            node_state_glyph(view.state),
            view.host.host_id,
            view.id.as_str()
        ),
        width,
    )
}

fn wrapped_line_count(text: &str, width: usize) -> u16 {
    let width = width.max(1);
    let mut lines = 1u16;
    let mut used = 0usize;
    for word in text.split_whitespace() {
        let word_width = word.chars().count();
        if used == 0 {
            used = word_width;
        } else if used + 1 + word_width <= width {
            used += 1 + word_width;
        } else {
            lines = lines.saturating_add(1);
            used = word_width;
        }
    }
    lines
}

/// The wave aggregate, rendered once at the top.
///
/// Counts only — see the module doc for why the cost term and `[cancel all]`
/// are absent rather than fabricated or inert.
#[must_use]
pub fn wave_aggregate(room: &OrchestrationRoom) -> String {
    let mut completed = 0usize;
    let mut failed = 0usize;
    let mut running = 0usize;
    for view in room.nodes().values() {
        match view.state {
            NodeState::Completed => completed += 1,
            NodeState::Failed => failed += 1,
            NodeState::Running => running += 1,
            _ => {}
        }
    }
    format!("wave \u{2713}{completed} \u{2717}{failed} \u{25b6}{running}")
}

/// Copy for each of the three zero-states. Distinguishable on purpose.
#[must_use]
pub fn zero_state_lines(zero: RoomZeroState) -> [&'static str; 2] {
    match zero {
        RoomZeroState::NotAttached => [
            "· not attached — this session composed no orchestration journal",
            "Nothing has been recorded, and nothing is being hidden.",
        ],
        RoomZeroState::NoNodes => [
            "· the room journal is readable and holds no nodes yet",
            "A durable node appears here the first time one is registered.",
        ],
        RoomZeroState::AllTerminal => [
            "· every node in this room has finished",
            "These rows are a replay of completed work, not live sessions.",
        ],
    }
}

/// The unrendered-arrival boundary copy. One spelling, two call sites.
fn newer_entries_line(count: usize) -> String {
    format!(
        "↓ {count} newer {} — reopen to fold them",
        if count == 1 { "entry" } else { "entries" }
    )
}

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    state: &mut RoomPanelState,
    selected: usize,
    focus: &crate::domain::models::FocusState,
    theme: &Theme,
) {
    Clear.render(area, buf);

    let is_focused = matches!(
        focus,
        crate::domain::models::FocusState::Sidebar {
            panel: crate::domain::models::visual::PanelType::Room,
            ..
        }
    );
    let border_style = if is_focused {
        Style::default().fg(theme.colors.accent)
    } else {
        Style::default().fg(theme.colors.fg_secondary)
    };
    // "as of", never "live".
    let title = match state.read_at_ms {
        Some(ms) => format!(" Durable Room · as of {} ", format_unix_millis(ms)),
        None => " Durable Room ".to_owned(),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style)
        .title_bottom(Span::styled(
            " read-only replay · j/k move · Ctrl+X R close ",
            Style::default().fg(theme.colors.fg_muted),
        ));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let muted = Style::default()
        .fg(theme.colors.fg_muted)
        .add_modifier(Modifier::ITALIC);
    let mut rendered_selected = selected;

    if let Some(error) = state.error.clone() {
        state.set_viewport_rows(inner.height as usize, &mut rendered_selected);
        Paragraph::new(vec![
            Line::from(Span::styled(
                "⚠ could not read the room journal",
                Style::default().fg(theme.colors.error),
            )),
            Line::from(Span::styled(error, Style::default().fg(theme.colors.error))),
            Line::from(Span::styled(
                "This is a read failure, not an empty room.",
                muted,
            )),
        ])
        .wrap(Wrap { trim: true })
        .render(inner, buf);
        return;
    }

    // An unattached session has no durable source from which an unknown row
    // could have come. A readable no-node journal continues through the normal
    // chrome path so forward-unknown records remain visible.
    if state.zero_state() == Some(RoomZeroState::NotAttached) {
        state.set_viewport_rows(inner.height as usize, &mut rendered_selected);
        let [headline, detail] = zero_state_lines(RoomZeroState::NotAttached);
        Paragraph::new(vec![
            Line::from(Span::styled(headline, muted)),
            Line::from(Span::styled(detail, muted)),
        ])
        .render(inner, buf);
        return;
    }

    let mut chrome: Vec<Line<'static>> = Vec::new();
    if let Some(room) = state.room.as_ref() {
        chrome.push(Line::from(Span::styled(
            truncate_to_width(&wave_aggregate(room), inner.width as usize),
            Style::default().fg(theme.colors.fg_primary),
        )));
    }
    chrome.push(Line::from(Span::styled(
        truncate_to_width(&format!("here: {}", state.host_id), inner.width as usize),
        muted,
    )));
    match state.zero_state() {
        Some(RoomZeroState::NoNodes) => {
            let [headline, detail] = zero_state_lines(RoomZeroState::NoNodes);
            chrome.push(Line::from(Span::styled(headline, muted)));
            chrome.push(Line::from(Span::styled(detail, muted)));
        }
        Some(RoomZeroState::AllTerminal) => {
            let [headline, _] = zero_state_lines(RoomZeroState::AllTerminal);
            chrome.push(Line::from(Span::styled(headline, muted)));
        }
        _ => {}
    }
    if state.newer_entries > 0 {
        chrome.push(Line::from(Span::styled(
            newer_entries_line(state.newer_entries),
            Style::default().fg(theme.colors.accent),
        )));
    }
    if state.unknown_records > 0 {
        chrome.push(Line::from(Span::styled(
            truncate_to_width(
                &format!("? {} × {UNKNOWN_RECORD_LABEL}", state.unknown_records),
                inner.width as usize,
            ),
            Style::default().fg(theme.colors.warning),
        )));
    }

    let footer_height =
        wrapped_line_count(STRUCTURAL_REPLAY_CLAIM, inner.width as usize).min(inner.height);
    let list_height = (inner.height as usize).saturating_sub(chrome.len() + footer_height as usize);
    state.set_viewport_rows(list_height.max(1), &mut rendered_selected);
    let rows: Vec<(crate::domain::models::AgentId, String)> = state
        .room
        .as_ref()
        .map(|room| {
            room.nodes()
                .values()
                .map(|view| {
                    (
                        view.id.clone(),
                        visible_node_row(view, inner.width as usize),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let viewport = list_height.max(1);
    let start = state.scroll_offset.min(rows.len().saturating_sub(viewport));
    let end = (start + viewport).min(rows.len());
    state.record_rendered_viewport(
        rows.get(start).map(|(id, _)| id.clone()),
        rows.get(rendered_selected).map(|(id, _)| id.clone()),
    );
    let items: Vec<ListItem<'static>> = rows[start..end]
        .iter()
        .enumerate()
        .map(|(offset, (_, row))| {
            let style = if start + offset == rendered_selected {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.colors.fg_primary)
            };
            ListItem::new(Line::from(Span::styled(row.clone(), style)))
        })
        .collect();

    let chrome_height = chrome.len() as u16;
    let list_area = Rect {
        y: inner.y + chrome_height,
        height: inner.height.saturating_sub(chrome_height + footer_height),
        ..inner
    };
    if list_area.height > 0 {
        ratatui::prelude::Widget::render(List::new(items), list_area, buf);
    }
    if chrome_height > 0 {
        let chrome_area = Rect {
            height: chrome_height.min(inner.height),
            ..inner
        };
        Paragraph::new(chrome).render(chrome_area, buf);
    }
    let footer_area = Rect {
        y: inner.y + inner.height.saturating_sub(footer_height),
        height: footer_height,
        ..inner
    };
    Paragraph::new(STRUCTURAL_REPLAY_CLAIM)
        .style(muted)
        .wrap(Wrap { trim: true })
        .render(footer_area, buf);

    // Paint the pending marker for this frame before acknowledging anything.
    // Only reaching the end of the folded row set advances the rendered
    // boundary; an observed-but-unfolded durable head remains pending.
    if list_area.height > 0 && end == rows.len() {
        state.acknowledge_rendered_boundary();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{AgentId, HostBinding, NodeOrigin, RoomEvent};

    fn room_with(state_change: Option<NodeState>, host: &str, current: &str) -> OrchestrationRoom {
        let node = AgentId::parse("spoke-a").expect("agent id");
        let mut events = vec![RoomEvent::NodeRegistered {
            node: node.clone(),
            origin: NodeOrigin::Interactive,
            host: HostBinding::new(host, "ws"),
        }];
        if let Some(to) = state_change {
            events.push(RoomEvent::NodeStateChanged {
                node,
                from: NodeState::Created,
                to,
            });
        }
        OrchestrationRoom::project_for_host(
            crate::domain::models::OrchestrationRoomId::parse("room-x").expect("room id"),
            events,
            current,
        )
    }

    fn painted(panel: &mut RoomPanelState, area: Rect) -> String {
        let mut buf = Buffer::empty(area);
        render(
            area,
            &mut buf,
            panel,
            0,
            &crate::domain::models::FocusState::Chat,
            &Theme::dark(),
        );
        buf.content()
            .chunks(area.width as usize)
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_host_bound_row_replaces_resumable_names_the_host_and_hazards() {
        let room = room_with(Some(NodeState::Suspended), "host-A", "host-B");
        let view = room.nodes().values().next().expect("one node");
        let row = node_row(view);
        assert!(row.contains("· host-bound: host-A"), "{row}");
        assert!(row.contains('▲'), "{row}");
        assert!(
            !row.contains("resumable"),
            "the suffix is REPLACED, not supplemented: {row}"
        );
        // The Suspended glyph is kept.
        assert!(row.contains('z'), "{row}");
        // Latched: no promise of recovery.
        let lowered = row.to_ascii_lowercase();
        for forbidden in ["recovering", "will retry", "reconnect", "resum"] {
            assert!(!lowered.contains(forbidden), "{row}");
        }
    }

    #[test]
    fn a_same_host_parked_spoke_still_reads_resumable() {
        let room = room_with(Some(NodeState::Suspended), "host-A", "host-A");
        let view = room.nodes().values().next().expect("one node");
        let row = node_row(view);
        assert!(row.contains("· resumable"), "{row}");
        assert!(!row.contains("host-bound"), "{row}");
        assert!(!row.contains('▲'), "{row}");
    }

    #[test]
    fn the_host_bound_reason_survives_a_long_id_at_minimum_sidebar_width() {
        let node = AgentId::parse("spoke-abcdefghijklmnopqrstuvwxyz").expect("agent id");
        let room = OrchestrationRoom::project_for_host(
            crate::domain::models::OrchestrationRoomId::parse("room-narrow").unwrap(),
            [
                RoomEvent::NodeRegistered {
                    node: node.clone(),
                    origin: NodeOrigin::Interactive,
                    host: HostBinding::new("host-A", "ws"),
                },
                RoomEvent::NodeStateChanged {
                    node,
                    from: NodeState::Created,
                    to: NodeState::Suspended,
                },
            ],
            "host-B",
        );
        let view = room.nodes().values().next().expect("one node");
        let cut = visible_node_row(view, 34);
        assert!(cut.contains("host-bound: host-A"), "{cut}");
        assert!(cut.contains('\u{25b2}'), "{cut}");
    }

    #[test]
    fn the_wave_aggregate_counts_only_and_fabricates_no_cost_or_action() {
        let room = room_with(Some(NodeState::Completed), "host-A", "host-A");
        let aggregate = wave_aggregate(&room);
        assert_eq!(aggregate, "wave ✓1 ✗0 ▶0");
        assert!(!aggregate.contains('$'), "{aggregate}");
        assert!(!aggregate.contains("cancel"), "{aggregate}");
    }

    #[test]
    fn the_three_zero_states_are_distinguishable() {
        let copies: Vec<&str> = [
            RoomZeroState::NotAttached,
            RoomZeroState::NoNodes,
            RoomZeroState::AllTerminal,
        ]
        .iter()
        .map(|zero| zero_state_lines(*zero)[0])
        .collect();
        let unique: std::collections::BTreeSet<&&str> = copies.iter().collect();
        assert_eq!(unique.len(), 3, "{copies:?}");
    }

    #[test]
    fn the_unknown_record_label_is_the_shipped_spelling() {
        assert_eq!(
            UNKNOWN_RECORD_LABEL,
            "unrecognised record — written by a newer build"
        );
    }
    #[test]
    fn unknown_only_journal_and_footer_are_visible_at_minimum_width() {
        let room = OrchestrationRoom::project_for_host(
            crate::domain::models::OrchestrationRoomId::parse("room-unknown").unwrap(),
            std::iter::empty(),
            "host-A",
        );
        let mut panel = RoomPanelState::default();
        let mut selected = 0;
        panel.apply_read(room, "host-A".to_owned(), 1, 1, 10, &mut selected);

        let text = painted(&mut panel, Rect::new(0, 0, 36, 18));
        assert!(text.contains("unrecognised record"), "{text}");
        assert!(text.contains("structurally replayable (not"), "{text}");
        assert!(text.contains("payload- or"), "{text}");
        assert!(text.contains("provenance-authenticated"), "{text}");

        panel.error = Some("disk went away".to_owned());
        let failed = painted(&mut panel, Rect::new(0, 0, 36, 18));
        assert!(failed.contains("disk went away"), "{failed}");
    }

    #[test]
    fn unseen_head_stays_pending_until_its_fold_is_rendered() {
        let room = room_with(Some(NodeState::Running), "host-A", "host-A");
        let mut panel = RoomPanelState::default();
        let mut selected = 0;
        panel.apply_read(room, "host-A".to_owned(), 1, 0, 10, &mut selected);

        let first = painted(&mut panel, Rect::new(0, 0, 36, 18));
        assert!(first.contains("1 newer entry"), "{first}");
        assert_eq!(panel.newer_entries, 0);

        assert!(panel.observe_head(2));
        let second = painted(&mut panel, Rect::new(0, 0, 36, 18));
        assert!(second.contains("1 newer entry"), "{second}");
        assert_eq!(
            panel.newer_entries, 1,
            "an observed head is not acknowledged by painting the older fold"
        );
    }

    #[test]
    fn refreshed_fold_rebases_room_anchors_by_node_identity() {
        let make_room = |names: &[&str]| {
            OrchestrationRoom::project_for_host(
                crate::domain::models::OrchestrationRoomId::parse("room-anchor").unwrap(),
                names.iter().map(|name| RoomEvent::NodeRegistered {
                    node: AgentId::parse(name).unwrap(),
                    origin: NodeOrigin::Interactive,
                    host: HostBinding::new("host-A", "ws"),
                }),
                "host-A",
            )
        };
        let mut panel = RoomPanelState::default();
        let mut selected = 1;
        panel.apply_read(
            make_room(&["b", "c", "d"]),
            "host-A".to_owned(),
            3,
            0,
            10,
            &mut selected,
        );
        panel.record_rendered_viewport(
            Some(AgentId::parse("b").unwrap()),
            Some(AgentId::parse("c").unwrap()),
        );
        panel.viewport_rows = 2;

        selected = 99;
        panel.apply_read(
            make_room(&["a", "b", "c", "d"]),
            "host-A".to_owned(),
            4,
            0,
            20,
            &mut selected,
        );
        assert_eq!(panel.scroll_offset, 1, "top row remains b");
        assert_eq!(selected, 2, "selected row remains c");

        let mut fresh = RoomPanelState::default();
        selected = 99;
        fresh.apply_read(
            make_room(&["a", "b"]),
            "host-A".to_owned(),
            2,
            0,
            10,
            &mut selected,
        );
        assert_eq!(selected, 1, "cross-panel selection is clamped");
    }
}
