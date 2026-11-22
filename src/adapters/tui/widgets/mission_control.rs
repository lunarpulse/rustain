//! The paint of the Mission Control frame (S-MC, Story 21.7): the title the compositor
//! supplies, the tab row, the tab body and the generated rail. Paint only: the frame rectangle
//! comes from the common compositor, the state is shared and read-only, and what the paint
//! measured (rows, viewport, the wide split, the keys the rail offered whole) is recorded in
//! the state's [`Fit`] so the keys clamp against the very rows painted here (height == paint)
//! and a key whose hint was not painted is inert.
//!
//! A list tab paints the rows [`tabs`] builds — master list, and on a wide frame the detail
//! preview of the selected row; the Room tab paints `room_panel::render_body`, the same
//! read-only replay the sidebar showed; Messages paints 21-8's log page through
//! `thread_body::build` (laid out once per width and filter generation, then sliced).

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use super::{agent_panel, room_panel, task_panel};
use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::Host;
use crate::adapters::tui::keymap::render::{rail, rail_arrows, rail_pair};
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::mission_control::{Fit, McList, McPane, McTab, agents, tabs, tasks};
use crate::adapters::tui::presentation::viewer::{self, ViewerCmd};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry;
use crate::domain::services::peer_text::sanitize_display_line;

/// How the frame's inner rows are shared out. One plan serves the paint and the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Plan {
    tab: u16,
    field: u16,
    rail: u16,
    body: u16,
}

/// The body keeps one row first. With the filter field open the field and the rail (its own
/// keys) outrank the tab row; otherwise the tab row outranks the rail, and a kept filter's
/// echo takes what is left.
fn plan(height: u16, editing: bool, kept: bool) -> Plan {
    let mut left = height.saturating_sub(1);
    let mut take = |wanted: bool| {
        let granted = u16::from(wanted && left > 0);
        left -= granted;
        granted
    };
    let (tab, field, rail);
    if editing {
        field = take(true);
        rail = take(true);
        tab = take(true);
    } else {
        tab = take(true);
        rail = take(true);
        field = take(kept);
    }
    Plan {
        tab,
        field,
        rail,
        body: height - tab - field - rail,
    }
}

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState, title: &str, host: Host) {
    let theme = &state.theme;
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(theme.colors.info_border))
            .style(
                Style::default()
                    .fg(theme.colors.fg_primary)
                    .bg(theme.colors.bg_surface),
            ),
        rect,
    );
    let inner = rect.inner(Margin::new(1, 1));
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mission = &state.mission_control;
    let search = mission.search();
    let editing = search.is_some_and(|search| search.editing);
    let kept = search.is_some_and(|search| !search.query.as_str().is_empty());
    let plan = plan(inner.height, editing, kept);
    let tab_area = Rect::new(inner.x, inner.y, inner.width, plan.tab);
    // The rows, top to bottom: tab row, body, filter echo, rail (the tab row sits above the
    // body even while the filter field outranks it for space).
    let body = Rect::new(inner.x, inner.y + plan.tab, inner.width, plan.body);
    let field_area = Rect::new(
        inner.x,
        inner.y + plan.tab + plan.body,
        inner.width,
        plan.field,
    );
    let rail_area = Rect::new(
        inner.x,
        inner.y + plan.tab + plan.body + plan.field,
        inner.width,
        plan.rail,
    );

    if plan.tab > 0 {
        frame.render_widget(tab_line(state, host), tab_area);
    }
    let mut fit = match mission.tab {
        McTab::Room => paint_room(frame, body, state),
        McTab::Messages => paint_messages(frame, body, state),
        McTab::Agents | McTab::Tasks | McTab::Artifacts | McTab::Probe => {
            paint_list(frame, body, state)
        }
    };
    if plan.field > 0
        && let Some(search) = search
    {
        let line = search.query_line(usize::from(field_area.width), theme, editing);
        frame.render_widget(
            Line::from(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style))
                    .collect::<Vec<_>>(),
            ),
            field_area,
        );
    }
    if plan.rail > 0 {
        let (text, painted) = rail_text(state, &fit, host, usize::from(rail_area.width));
        fit.search = painted.search;
        fit.export = painted.export;
        fit.open = painted.open;
        fit.thread = painted.thread;
        fit.pause = painted.pause;
        fit.kill = painted.kill;
        fit.skip = painted.skip;
        fit.copy = painted.copy;
        frame.render_widget(
            Line::styled(text, Style::default().fg(theme.colors.fg_secondary)),
            rail_area,
        );
    }
    mission.set_fit(fit);
}

/// The tab row: the delivered tabs in fixed order, the current one bold and marked, the others
/// muted. One tab (the attached host) paints no choice to make.
fn tab_line(state: &TuiState, host: Host) -> Line<'static> {
    let theme = &state.theme;
    let mission = &state.mission_control;
    let mut spans = Vec::with_capacity(6);
    for (index, tab) in mission.delivered(host).as_slice().iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        if *tab == mission.tab {
            spans.push(Span::styled(
                format!("▸{}", tab.label()),
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!(" {}", tab.label()),
                Style::default().fg(theme.colors.fg_muted),
            ));
        }
    }
    Line::from(spans)
}

fn paint_room(frame: &mut Frame<'_>, body: Rect, state: &TuiState) -> Fit {
    let paint = room_panel::render_body(
        body,
        frame.buffer_mut(),
        &state.room_panel,
        state.mission_control.room_cursor,
        &state.theme,
    );
    let fit = Fit {
        list_viewport: paint.viewport_rows,
        scrollable: state.room_panel.visible_len() > paint.viewport_rows,
        ..Fit::default()
    };
    state.mission_control.record_room_paint(paint);
    fit
}

/// 21-8's log page: the rows the shared page functions lay out, sliced at the viewer's top.
/// What is painted is what the keys clamp against, and an unfiltered body painted with at
/// least one row is the mark commit-on-present consumes (A13).
fn paint_messages(frame: &mut Frame<'_>, body: Rect, state: &TuiState) -> Fit {
    let theme = &state.theme;
    let Some(page) = state.mission_control.messages.as_ref() else {
        frame.render_widget(
            Line::styled(
                match &state.mission_control.messages_failed {
                    Some(error) => format!(
                        "Could not read the A2A log: {}",
                        sanitize_display_line(error)
                    ),
                    None => "Reading the A2A log…".to_owned(),
                },
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::ITALIC),
            ),
            body,
        );
        return Fit::default();
    };
    let lines = page.body(theme, body.width);
    let rows = lines.len();
    let viewport = usize::from(body.height);
    page.set_fit(rows, viewport);
    if viewport >= 1 && rows > 0 {
        page.mark_painted();
    }
    // `follow` is a pending jump-to-end (`G`): resolved here against the live fit.
    let mut seen = page.viewer;
    if seen.follow {
        seen.top = usize::MAX;
    }
    let range = viewer::slice(&seen, rows, viewport);
    for (offset, line) in lines[range].iter().enumerate() {
        frame.render_widget(
            line,
            Rect::new(body.x, body.y + offset as u16, body.width, 1),
        );
    }
    Fit {
        list_rows: rows,
        list_viewport: viewport,
        scrollable: rows > viewport,
        ..Fit::default()
    }
}

fn paint_list(frame: &mut Frame<'_>, area: Rect, state: &TuiState) -> Fit {
    let theme = &state.theme;
    let mission = &state.mission_control;
    let rows = mission.rows();
    let selection = mission.selection();
    let resolved = mission.resolved();
    let total = tabs::view_len(rows.len(), resolved);
    let muted = Style::default()
        .fg(theme.colors.fg_muted)
        .add_modifier(Modifier::ITALIC);
    // The note (one muted row above the rows) only where a row is left for the list.
    let note = mission
        .list()
        .and_then(|list| list.note.as_deref())
        .filter(|_| area.height >= 2);
    let body = if note.is_some() {
        Rect::new(area.x, area.y + 1, area.width, area.height - 1)
    } else {
        area
    };
    if let Some(note) = note {
        frame.render_widget(
            Line::styled(
                text_geometry::clip(&sanitize_display_line(note), usize::from(area.width))
                    .to_owned(),
                muted,
            ),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    let split = tabs::split(body.width);
    let master_width = split.map_or(body.width, |(master, _)| master);
    let master = Rect::new(body.x, body.y, master_width, body.height);
    let viewport = usize::from(body.height);
    let top = tabs::keep_visible(
        mission.fit().list_top,
        tabs::view_pos(resolved),
        viewport,
        total,
    );
    if total == 0 {
        let words = if mission.filter_kept() {
            "No row matches this filter."
        } else {
            mission
                .list()
                .and_then(|list| list.empty.as_deref())
                .unwrap_or("Nothing here yet.")
        };
        let safe = sanitize_display_line(words);
        for (offset, row) in text_geometry::wrap_hard(&safe, usize::from(master.width).max(1))
            .into_iter()
            .take(usize::from(master.height))
            .enumerate()
        {
            frame.render_widget(
                Line::styled(row.to_owned(), muted),
                Rect::new(master.x, master.y + offset as u16, master.width, 1),
            );
        }
    }
    for offset in 0..viewport.min(total.saturating_sub(top)) {
        let Some(row) = tabs::view_row(rows, selection, resolved, top + offset) else {
            break;
        };
        let (marker, style) = if row.gone {
            ("▸ ", muted)
        } else if row.selected {
            (
                "▸ ",
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", Style::default().fg(theme.colors.fg_primary))
        };
        // Untrusted text is sanitized at the span builder, never left to the backend, and a
        // row too wide for the master column keeps the parts that must survive truncation.
        let text = fit_row(&row, usize::from(master.width).saturating_sub(2));
        let line = Line::from(vec![Span::styled(format!("{marker}{text}"), style)]);
        frame.render_widget(
            line,
            Rect::new(master.x, master.y + offset as u16, master.width, 1),
        );
    }
    let mut fit = Fit {
        list_rows: total,
        list_viewport: viewport,
        list_top: top,
        wide: split.is_some(),
        ..Fit::default()
    };
    if let Some((_, detail_width)) = split {
        let detail_area = Rect::new(body.x + master_width + 1, body.y, detail_width, body.height);
        let faint = Style::default().fg(theme.colors.fg_muted);
        for y in body.y..body.bottom() {
            frame.render_widget(
                Line::styled("│", faint),
                Rect::new(body.x + master_width, y, 1, 1),
            );
        }
        let lines = mission.detail_rows(usize::from(detail_width));
        let slice = viewer::slice(
            &mission.detail,
            lines.len(),
            usize::from(detail_area.height),
        );
        for (offset, text) in lines[slice].iter().enumerate() {
            frame.render_widget(
                Line::styled(text.clone(), Style::default().fg(theme.colors.fg_primary)),
                Rect::new(
                    detail_area.x,
                    detail_area.y + offset as u16,
                    detail_area.width,
                    1,
                ),
            );
        }
        fit.detail_rows = lines.len();
        fit.detail_viewport = usize::from(detail_area.height);
    }
    fit.scrollable = match (mission.pane, fit.wide) {
        (McPane::Detail, true) => fit.detail_rows > fit.detail_viewport,
        _ => total > 1,
    };
    fit
}

/// One list row at `width` cells: the sanitized text, cut at the width; when the cut would
/// lose a part the row says must survive (a decision, a hazard), its narrow reordering.
fn fit_row(row: &tabs::ViewRow<'_>, width: usize) -> String {
    let text = sanitize_display_line(&row.text);
    let cut = text_geometry::clip(&text, width);
    let lost = row.keep.iter().any(|part| !cut.contains(part.as_str()));
    match row.compact {
        Some(compact) if cut.len() < text.len() && lost => {
            text_geometry::clip(&sanitize_display_line(compact), width).to_owned()
        }
        _ => cut.to_owned(),
    }
}

/// Which keyed hints the rail painted whole.
#[derive(Debug, Clone, Copy, Default)]
struct Painted {
    search: bool,
    export: bool,
    open: bool,
    thread: bool,
    pause: bool,
    kill: bool,
    skip: bool,
    copy: bool,
}

/// The keyed hints of the rail whose keys are inert unless the hint was painted.
#[derive(Debug, Clone, Copy)]
enum Hint {
    Search,
    Export,
    Open,
    Thread,
    Pause,
    Kill,
    Skip,
    Copy,
}

/// The rail, generated from the registry rows: a key is painted only where it acts (a second
/// tab, rows to move through, a wide frame, a filter, an export, a row to open); what does
/// not fit is dropped from the right, the way out always stays. The returned facts say which
/// keyed hints survived: those keys are live, the others inert.
fn rail_text(state: &TuiState, fit: &Fit, host: Host, width: usize) -> (String, Painted) {
    if crate::adapters::tui::mission_control::reordering(state) {
        return (reorder_rail(), Painted::default());
    }
    let mission = &state.mission_control;
    let mut items: Vec<(String, Option<Hint>)> = Vec::with_capacity(6);
    let plain = |text: String| (text, None);
    if fit.scrollable {
        items.push(plain(format!(
            "[{}] {}",
            rail_pair(
                KeyContext::InspectorBody,
                ViewerCmd::LineDown,
                ViewerCmd::LineUp
            ),
            if mission.tab.is_list() && !(mission.pane == McPane::Detail && fit.wide) {
                "Move"
            } else {
                "Scroll"
            }
        )));
    }
    if fit.wide && mission.tab.is_list() {
        items.push(plain(format!(
            "{} {}",
            rail(KeyContext::MissionControl, MissionCmd::Focus),
            if mission.pane == McPane::Master {
                "Detail"
            } else {
                "List"
            }
        )));
    }
    // Row actions come before the filter and the export: the narrow rail drops from the right.
    if matches!(mission.tab, McTab::Artifacts | McTab::Agents | McTab::Tasks)
        && host == Host::Local
        && mission.selected_id().is_some()
    {
        items.push((
            format!(
                "{} Open",
                rail(KeyContext::MissionControl, MissionCmd::Open)
            ),
            Some(Hint::Open),
        ));
    }
    // The peer thread of a delegation row follows `Enter Open` (so it outlives `p` and `x` on
    // a narrow rail); while its lookup is pending the hint is plain text and the key is inert.
    if mission.tab == McTab::Agents && host == Host::Local && mission.selected_id().is_some() {
        if agents::thread_pending(state) {
            items.push(("opening thread…".to_owned(), None));
        } else if agents::thread_offer(state).is_some() {
            items.push((
                format!(
                    "{} Open thread",
                    rail(KeyContext::MissionControl, MissionCmd::Thread)
                ),
                Some(Hint::Thread),
            ));
        }
    }
    if mission.tab == McTab::Tasks
        && host == Host::Local
        && let Some(status) = tasks::selected_status(state)
    {
        if let Some(label) = task_panel::pause_label(status) {
            items.push((
                format!(
                    "{} {label}",
                    rail(KeyContext::MissionControl, MissionCmd::Pause)
                ),
                Some(Hint::Pause),
            ));
        }
        if task_panel::can_skip(status) {
            items.push((
                format!(
                    "{} Skip",
                    rail(KeyContext::MissionControl, MissionCmd::Skip)
                ),
                Some(Hint::Skip),
            ));
        }
        if mission
            .tasks
            .as_ref()
            .is_some_and(|source| source.plan_executing)
        {
            items.push((
                format!(
                    "{} Cancel plan",
                    rail(KeyContext::MissionControl, MissionCmd::Kill)
                ),
                Some(Hint::Kill),
            ));
        }
    }
    if mission.tab == McTab::Agents
        && host == Host::Local
        && let Some(entry) = agents::selected(state)
    {
        if let Some(label) = agent_panel::pause_label(entry.current_status) {
            items.push((
                format!(
                    "{} {label}",
                    rail(KeyContext::MissionControl, MissionCmd::Pause)
                ),
                Some(Hint::Pause),
            ));
        }
        if agent_panel::can_kill(entry) {
            items.push((
                format!(
                    "{} Kill",
                    rail(KeyContext::MissionControl, MissionCmd::Kill)
                ),
                Some(Hint::Kill),
            ));
        }
    }
    let searchable = match mission.tab {
        McTab::Messages => mission.messages.is_some(),
        tab if tab.is_list() => mission.list().is_some_and(McList::has_rows),
        _ => false,
    };
    if searchable {
        items.push((
            format!(
                "{} Search",
                rail(KeyContext::MissionControl, OverlayCmd::InspectorSearch)
            ),
            Some(Hint::Search),
        ));
    }
    if mission.tab == McTab::Messages && mission.messages.is_some() {
        let exportable = host == Host::Local
            && mission
                .messages
                .as_ref()
                .is_some_and(|page| page.report.is_some());
        if exportable {
            items.push((
                format!(
                    "{} Export",
                    rail(KeyContext::MissionControl, MissionCmd::Export)
                ),
                Some(Hint::Export),
            ));
        }
    }
    // `c` Copy follows the filter: the narrow rail drops it (and its inert key) before them.
    if mission.tab.is_list() && host == Host::Local && mission.selected_id().is_some() {
        items.push((
            format!(
                "{} Copy",
                rail(KeyContext::MissionControl, MissionCmd::CopyRow)
            ),
            Some(Hint::Copy),
        ));
    }
    // The tab keys go last: a narrow rail drops from the right, and the row actions and the
    // filter matter more than a hint for keys that stay live either way.
    if mission.delivered(host).as_slice().len() >= 2 {
        items.push(plain(format!(
            "{} Tab",
            rail_arrows(
                KeyContext::MissionControl,
                MissionCmd::PrevTab,
                MissionCmd::NextTab
            )
        )));
    }
    let back = if search_open(state) {
        format!(
            "{} Keep filter  {} Clear",
            rail(KeyContext::InspectorSearch, OverlayCmd::InspectorAccept),
            rail(KeyContext::InspectorSearch, OverlayCmd::InspectorBack),
        )
    } else {
        format!(
            "{} Back",
            rail(KeyContext::MissionControl, MissionCmd::Back)
        )
    };
    if search_open(state) {
        // The open field is a text context with its own keys: nothing else is offered.
        return (back, Painted::default());
    }
    loop {
        let mut text = items
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("  ");
        if !text.is_empty() {
            text.push_str("  ");
        }
        text.push_str(&back);
        if items.is_empty() || text_geometry::cells(&text) <= width {
            let mut painted = Painted::default();
            for (_, mark) in &items {
                match mark {
                    Some(Hint::Search) => painted.search = true,
                    Some(Hint::Export) => painted.export = true,
                    Some(Hint::Open) => painted.open = true,
                    Some(Hint::Thread) => painted.thread = true,
                    Some(Hint::Pause) => painted.pause = true,
                    Some(Hint::Kill) => painted.kill = true,
                    Some(Hint::Skip) => painted.skip = true,
                    Some(Hint::Copy) => painted.copy = true,
                    None => {}
                }
            }
            return (text, painted);
        }
        items.pop();
    }
}

/// The rail while a task is being reordered: the move keys, the commit and the cancel; nothing
/// else acts (the reorder context has no other row).
fn reorder_rail() -> String {
    format!(
        "[{}] Move  {} Commit  {} Cancel",
        rail_pair(
            KeyContext::MissionReorder,
            MissionCmd::ReorderDown,
            MissionCmd::ReorderUp
        ),
        rail(KeyContext::MissionReorder, MissionCmd::ReorderCommit),
        rail(KeyContext::MissionReorder, MissionCmd::ReorderCancel),
    )
}

fn search_open(state: &TuiState) -> bool {
    crate::adapters::tui::mission_control::search_editing(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::app::InputAction;
    use crate::adapters::tui::keymap::Key;
    use crate::adapters::tui::mission_control::keys;
    use crate::adapters::tui::presentation::reducer;
    use crate::adapters::tui::state::AgentThreadOpening;
    use crate::domain::models::subagent_view::{AgentRowView, OwnershipKind};
    use crate::domain::models::{AgentId, NodeState, SentThreadOffer};
    use ratatui::{Terminal, backend::TestBackend};

    const HEIGHT: u16 = 24;

    /// A running delegation row (a peer node, as the model-rail delegation registers it).
    fn delegation_row() -> AgentRowView {
        AgentRowView {
            isolated: false,
            agent_id: AgentId::new(),
            parent_id: AgentId::root(),
            subagent_type: "a2a-peer".to_owned(),
            spawned_at: 0,
            depth: 1,
            current_status: NodeState::Running,
            ownership: OwnershipKind::Peer,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            wait_reason: None,
        }
    }

    /// Mission Control on Agents with one running delegation row selected; `offered` gives the
    /// row a verified peer exchange.
    fn agents_frame(offered: bool) -> (TuiState, AgentId) {
        let mut state = TuiState::new(80, HEIGHT);
        let row = delegation_row();
        let id = row.agent_id.clone();
        state.agent_panel_state.cached_entries = vec![row];
        agents::sync(&mut state);
        reducer::open_mission_control(&mut state, McTab::Agents);
        state.mission_control.move_selection(ViewerCmd::LineDown);
        assert_eq!(state.mission_control.selected_id(), Some(id.as_str()));
        if offered {
            let offer = SentThreadOffer {
                peer: "marcus".to_owned(),
                task: "task-1".to_owned(),
            };
            state.agent_threads.offers.insert(id.clone(), offer);
        }
        (state, id)
    }

    /// The rail (the last inner row) of the frame painted at `outer` columns for `host`.
    fn painted_rail(state: &TuiState, outer: u16, host: Host) -> String {
        let mut terminal = Terminal::new(TestBackend::new(outer, HEIGHT)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), state, "Agents", host))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (1..outer - 1)
            .map(|x| buffer[(x, HEIGHT - 2)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn agents_thread_the_rail_paints_open_thread_right_after_open_and_t_follows_it() {
        for outer in [80, 130] {
            let (mut state, id) = agents_frame(true);
            let rail = painted_rail(&state, outer, Host::Local);
            assert!(rail.contains("[Enter] Open  [t] Open thread"), "{rail}");
            let thread = rail.find("[t] Open thread").unwrap();
            let pause = rail.find("[p] Pause").unwrap();
            assert!(thread < pause, "{rail}");
            assert!(rail.contains("[x] Kill"), "{rail}");
            assert!(state.mission_control.fit().thread);
            assert_eq!(
                keys::on_key(&mut state, Key::Char('t'), Host::Local),
                InputAction::OpenAgentThread(id)
            );
        }
    }

    #[test]
    fn agents_thread_a_row_without_a_verified_offer_paints_no_hint_and_t_is_inert() {
        let (mut state, _) = agents_frame(false);
        let rail = painted_rail(&state, 80, Host::Local);
        assert!(!rail.contains("Open thread"), "{rail}");
        assert!(rail.contains("[Enter] Open  [p] Pause"), "{rail}");
        assert!(!state.mission_control.fit().thread);
        assert_eq!(
            keys::on_key(&mut state, Key::Char('t'), Host::Local),
            InputAction::Consumed
        );
    }

    #[test]
    fn agents_thread_a_pending_lookup_paints_opening_thread_and_t_is_inert() {
        let (mut state, id) = agents_frame(true);
        let opening = AgentThreadOpening {
            agent: id,
            generation: 1,
        };
        state.agent_threads.opening = Some(opening.clone());
        let rail = painted_rail(&state, 80, Host::Local);
        assert!(rail.contains("[Enter] Open  opening thread…"), "{rail}");
        assert!(!rail.contains("[t]"), "{rail}");
        assert!(!state.mission_control.fit().thread);
        assert_eq!(
            keys::on_key(&mut state, Key::Char('t'), Host::Local),
            InputAction::Consumed
        );
        assert_eq!(state.agent_threads.opening, Some(opening));
    }

    #[test]
    fn agents_thread_a_rail_too_narrow_for_the_hint_leaves_t_inert() {
        let (mut state, id) = agents_frame(true);
        // 28 inner columns: `[Enter] Open  [Esc] Back` (24 cells) fits; with
        // `[t] Open thread  ` (17 more) it would not, so the hint is dropped from the right.
        let rail = painted_rail(&state, 30, Host::Local);
        assert_eq!(rail, "[Enter] Open  [Esc] Back");
        assert!(state.mission_control.fit().open);
        assert!(!state.mission_control.fit().thread);
        assert_eq!(
            keys::on_key(&mut state, Key::Char('t'), Host::Local),
            InputAction::Consumed
        );
        // Painted whole again, the same key acts.
        painted_rail(&state, 80, Host::Local);
        assert_eq!(
            keys::on_key(&mut state, Key::Char('t'), Host::Local),
            InputAction::OpenAgentThread(id)
        );
    }

    #[test]
    fn agents_thread_the_attached_host_paints_no_thread_hint_and_has_no_t() {
        let (mut state, _) = agents_frame(true);
        let rail = painted_rail(&state, 80, Host::Attached);
        assert!(!rail.contains("Open thread"), "{rail}");
        assert!(!state.mission_control.fit().thread);
        assert_eq!(
            keys::on_key(&mut state, Key::Char('t'), Host::Attached),
            InputAction::Consumed
        );
    }
}
