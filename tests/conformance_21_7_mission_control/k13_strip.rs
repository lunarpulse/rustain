//! K13 / AC27 / A22 (21-7 slice 7-G): the mission strip. One muted row directly above the status
//! row in the main column at every width and height, never covered by a frame, sheet, popup or
//! toast, yielding its row to the Stage exactly when reserving it would leave the Stage fewer than
//! five rows (the 21-4 / 21-6 frame floor); the grammar `Agents r/n · Tasks d/n · Decisions n`
//! with the mirror `Writer decisions n`, the attached host omitting what it does not project, the
//! generated idle hint; no text of a peer, model or tool ever reaches a cell.
//!
//! Every paint goes through the production boundary both hosts call (`presentation::compose`, with
//! `Host::Local` and `Host::Attached`) or the production local draw (`event_loop::render` through
//! the `Rig`); counts come from the production producers (`apply_roster`, the decision producers,
//! a real plan in the active conversation). The attached socket host's own paint has an in-file
//! witness in `attach_loop.rs`. No sleeps.
use super::support::*;
use crate::conformance_21_6_decision_queue::support::{
    clocked, facts, layout_of, paint, route, row_of, row_text, screen, special,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use rustain::adapters::tui::decision_queue;
use rustain::adapters::tui::handlers::context_warning::handle_upsert_context_warning;
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::layout::{AppLayout, MIN_FRAME_ROWS, compute_layout};
use rustain::adapters::tui::notifications::{self, NoticeSource};
use rustain::adapters::tui::presentation::{self, Shell};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::mission_strip::{self, DecisionCount, MissionFacts};
use rustain::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus};
use rustain::domain::models::{
    AgentId, AgentRowView, Conversation, FocusState, NodeState, OwnershipKind, TabManager,
};
use rustain::infrastructure::runtime::mission_bridge;

/// The words the native drivers wait on: the strip must never contain them (A22, I5).
const DRIVER_WORDS: [&str; 5] = ["Ready", "Needs you", "Read-only.", "Decision:", "waiting"];

fn task(number: u32, status: PlanTaskStatus) -> PlanTask {
    PlanTask {
        number,
        title: format!("task {number}"),
        description: String::new(),
        depends_on: vec![],
        status,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    }
}

fn plan(title: &str) -> Plan {
    Plan {
        id: "p1".to_owned(),
        title: title.to_owned(),
        tasks: vec![
            task(1, PlanTaskStatus::Completed),
            task(2, PlanTaskStatus::Running),
            task(3, PlanTaskStatus::Pending),
        ],
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1,
        resolved_at: None,
        host_message_id: None,
    }
}

/// The active conversation holding a three-task plan, one task done.
fn conversation(plan_title: &str) -> Conversation {
    let tabs = TabManager::default();
    let mut conversation = tabs.active_tab().conversation.clone();
    conversation.plans.insert("p1".to_owned(), plan(plan_title));
    conversation
}

fn agent(name: &str, status: NodeState) -> AgentRowView {
    AgentRowView {
        agent_id: AgentId::new(),
        parent_id: AgentId::root(),
        subagent_type: name.to_owned(),
        spawned_at: 0,
        depth: 1,
        current_status: status,
        ownership: OwnershipKind::Owned,
        effective_model: String::new(),
        tools_summary: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        turns: 0,
        isolated: false,
        wait_reason: None,
    }
}

/// A `width`x`height` frame painted by the shared compositor of `host`: distinct Stage rows,
/// the composer and status sentinels, then exactly the host's `compose` call.
fn draw(
    state: &mut TuiState,
    host: Host,
    stage: Option<&Conversation>,
    width: u16,
    height: u16,
) -> (Buffer, AppLayout) {
    let layout = layout_of(state, width, height);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let log = facts("c1");
    {
        let state: &TuiState = state;
        terminal
            .draw(|frame| {
                let pane = layout.chat_pane;
                let rows: Vec<String> = (0..pane.height)
                    .map(|row| {
                        let tag = format!("stage row {row:02} ");
                        tag.repeat(usize::from(pane.width) / tag.len() + 1)
                    })
                    .collect();
                frame.render_widget(Paragraph::new(rows.join("\n")), pane);
                frame.render_widget(Paragraph::new("composer-sentinel"), layout.input_area);
                let mut shell = Shell::new(frame.area(), &layout);
                shell.stage = stage.map(|conversation| (conversation, &[][..]));
                presentation::compose(
                    frame,
                    state,
                    &shell,
                    host,
                    &log,
                    &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                    |frame| {
                        frame.render_widget(
                            Paragraph::new("status-row-sentinel"),
                            layout.status_bar,
                        );
                    },
                );
            })
            .unwrap();
    }
    (terminal.backend().buffer().clone(), layout)
}

/// The strip row of a drawn frame, trailing blanks trimmed.
fn strip_row(buffer: &Buffer, layout: &AppLayout) -> String {
    let strip = layout.mission_strip.expect("the strip is laid out");
    row_text(buffer, strip.y).trim_end().to_owned()
}

fn draft(lines: usize) -> String {
    vec!["draft line"; lines].join("\n")
}

#[test]
fn the_strip_row_sits_directly_above_the_status_row_in_the_main_column_at_every_size() {
    let theme = Theme::dark();
    for width in (60u16..=200).step_by(7).chain([119, 120, 121, 200]) {
        for height in (16u16..=60).step_by(3).chain([17, 24, 30]) {
            for monitor in [false, true] {
                for lines in [0usize, 1, 4, 7, 8, 12] {
                    let area = Rect::new(0, 0, width, height);
                    let layout = compute_layout(area, &theme, &draft(lines), 1, monitor).unwrap();
                    let at = format!("{width}x{height} monitor={monitor} draft={lines}");
                    let reserved = u16::from(layout.mission_strip.is_some());
                    assert_eq!(layout.status_bar.bottom(), layout.input_area.y, "{at}");
                    assert_eq!(
                        layout.chat_pane.bottom() + reserved,
                        layout.status_bar.y,
                        "{at}: the Stage, the strip and the status row stack without a gap"
                    );
                    if let Some(strip) = layout.mission_strip {
                        assert_eq!(strip.height, 1, "{at}");
                        assert_eq!(strip.y + 1, layout.status_bar.y, "{at}: row == status - 1");
                        assert_eq!(
                            (strip.x, strip.width),
                            (layout.status_bar.x, layout.status_bar.width),
                            "{at}: the strip lives in the main column"
                        );
                    }
                    // The yield rule: reserved iff the Stage keeps at least the frame floor.
                    assert_eq!(
                        layout.mission_strip.is_some(),
                        layout.chat_pane.height + reserved > MIN_FRAME_ROWS,
                        "{at}"
                    );
                }
            }
        }
    }
}

#[test]
fn full_composer_at_60x16_keeps_the_five_row_frame() {
    // 7 draft lines: the strip still fits (the Stage keeps exactly five rows); from 8 lines
    // on it yields and the Stage keeps five rows. A frame paints in both.
    for (lines, strip_expected) in [(0usize, true), (7, true), (8, false), (12, false)] {
        let (mut state, _clock) = clocked(60, 16);
        state.input_buffer = draft(lines);
        state.focus = FocusState::Chat;
        handle_upsert_context_warning(&mut state, "c1", 85);
        let (buffer, layout) = draw(&mut state, Host::Local, None, 60, 16);
        let at = format!("draft={lines}");
        assert_eq!(layout.mission_strip.is_some(), strip_expected, "{at}");
        if lines >= 7 {
            assert_eq!(layout.chat_pane.height, MIN_FRAME_ROWS, "{at}: the floor");
        }
        let text = screen(&buffer);
        assert!(
            row_of(&buffer, "Decision: Context window").is_some(),
            "{at}: the five-row frame paints\n{text}"
        );
        assert!(
            row_of(&buffer, "[K] Compact").is_some(),
            "{at}: its rail\n{text}"
        );
        assert_eq!(
            row_text(&buffer, layout.status_bar.y).trim_end(),
            "status-row-sentinel",
            "{at}"
        );
        if !strip_expected {
            assert!(
                !text.contains("Mission Control") && !text.contains("Decisions 1"),
                "{at}: a yielded strip paints nowhere\n{text}"
            );
        }
    }
}

#[test]
fn the_strip_is_never_covered_by_a_frame_or_a_menu() {
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        for host in [Host::Local, Host::Attached] {
            let (mut state, _clock) = clocked(width, height);
            state.focus = FocusState::Chat;
            // A decision frame on the stack (a full sheet below 100 columns), then the leader
            // menu (which-key) over it.
            handle_upsert_context_warning(&mut state, "c1", 85);
            state.pending_leader = Some(rustain::adapters::tui::keymap::Context::Chat);
            let (buffer, layout) = draw(&mut state, host, None, width, height);
            let at = format!("{width}x{height} {host:?}");
            let row = strip_row(&buffer, &layout);
            assert_eq!(row, "Decisions 1", "{at}: {row:?}\n{}", screen(&buffer));
            assert!(
                row_of(&buffer, "Decision: Context window").is_some_and(
                    |top| top < layout.mission_strip.expect("the Stage is tall enough").y
                ),
                "{at}: the frame paints above the strip\n{}",
                screen(&buffer)
            );
            assert_eq!(
                row_text(&buffer, layout.status_bar.y).trim_end(),
                "status-row-sentinel",
                "{at}"
            );
        }
    }
}

#[test]
fn a_toast_anchors_to_the_pane_bottom_directly_above_the_strip() {
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        for host in [Host::Local, Host::Attached] {
            let (mut state, _clock) = clocked(width, height);
            notifications::info(&mut state, NoticeSource::Session, "a toast over the stage");
            let (buffer, layout) = draw(&mut state, host, None, width, height);
            let at = format!("{width}x{height} {host:?}");
            let strip = layout.mission_strip.expect("the Stage is tall enough");
            assert_eq!(
                strip_row(&buffer, &layout),
                match host {
                    Host::Local => "^X s Mission Control",
                    Host::Attached => "^X l Messages",
                },
                "{at}: the toast never covers the strip\n{}",
                screen(&buffer)
            );
            // Narrow: one borderless row on the pane's last row; wide: a bordered box whose text
            // row sits above its bottom border. Either way it ends at the row above the strip.
            let last = row_text(&buffer, strip.y - 1);
            assert!(
                last.contains("a toast over the stage")
                    || (width >= 100 && (last.contains('┘') || last.contains('╯'))),
                "{at}: the toast is anchored on the pane bottom: {last:?}\n{}",
                screen(&buffer)
            );
        }
    }
}

#[test]
fn the_grammar_holds_for_local_mirror_and_idle_rows() {
    let conversation = conversation("Release");
    let (mut state, _clock) = clocked(80, 24);
    state.task_panel_state.last_executed_plan_id = Some("p1".to_owned());
    // Idle: no roster, no plan on the Stage, nothing waiting: the generated hint.
    let (buffer, layout) = draw(&mut state, Host::Local, None, 80, 24);
    assert_eq!(strip_row(&buffer, &layout), "^X s Mission Control");
    let (buffer, layout) = draw(&mut state, Host::Attached, None, 80, 24);
    assert_eq!(strip_row(&buffer, &layout), "^X l Messages");
    // Tasks alone (a resolved plan of the active conversation).
    let (buffer, layout) = draw(&mut state, Host::Local, Some(&conversation), 80, 24);
    assert_eq!(strip_row(&buffer, &layout), "Tasks 1/3");
    // All three segments, in order.
    mission_bridge::apply_roster(
        &mut state,
        vec![
            agent("reviewer", NodeState::Running),
            agent("scout", NodeState::Completed),
        ],
    );
    handle_upsert_context_warning(&mut state, "c1", 85);
    state.focus = FocusState::Chat;
    let (buffer, layout) = draw(&mut state, Host::Local, Some(&conversation), 80, 24);
    assert_eq!(
        strip_row(&buffer, &layout),
        "Agents 1/2 \u{B7} Tasks 1/3 \u{B7} Decisions 1"
    );
    // The count is the status chip's: the decision queue length, not a second count.
    assert_eq!(decision_queue::waiting(&state), 1);
    // The read-only mirror names the writer; the attached host omits agents and tasks.
    let (mut mirror, _clock) = clocked(80, 24);
    mirror.decisions.set_mirror_waiting(2);
    let (buffer, layout) = draw(&mut mirror, Host::Attached, None, 80, 24);
    assert_eq!(strip_row(&buffer, &layout), "Writer decisions 2");
    // Narrow labels and the right-to-left drop come from the same builder.
    let all = MissionFacts {
        agents: Some((1, 3)),
        tasks: Some((4, 7)),
        decisions: Some(DecisionCount::Waiting(2)),
    };
    assert_eq!(
        mission_strip::text(&all, Host::Local, 80),
        "Agents 1/3 \u{B7} Tasks 4/7 \u{B7} Decisions 2"
    );
    assert_eq!(
        mission_strip::text(&all, Host::Local, 30),
        "A 1/3 \u{B7} T 4/7 \u{B7} D 2"
    );
    assert_eq!(
        mission_strip::text(&all, Host::Local, 14),
        "A 1/3 \u{B7} T 4/7"
    );
}

#[test]
fn attached_strip_omits_unprojected_segments() {
    let conversation = conversation("Release");
    let (mut state, _clock) = clocked(80, 24);
    state.task_panel_state.last_executed_plan_id = Some("p1".to_owned());
    mission_bridge::apply_roster(&mut state, vec![agent("reviewer", NodeState::Running)]);
    handle_upsert_context_warning(&mut state, "c1", 85);
    state.focus = FocusState::Chat;
    let local = mission_strip::facts(&state, Host::Local, Some(&conversation));
    assert_eq!(local.agents, Some((1, 1)));
    assert_eq!(local.tasks, Some((1, 3)));
    assert_eq!(local.decisions, Some(DecisionCount::Waiting(1)));
    // The attached host projects no agents or tasks (A20): they are absent, never `0/0`.
    let attached = mission_strip::facts(&state, Host::Attached, Some(&conversation));
    assert_eq!(attached.agents, None);
    assert_eq!(attached.tasks, None);
    assert_eq!(attached.decisions, Some(DecisionCount::Waiting(1)));
    let (buffer, layout) = draw(&mut state, Host::Attached, Some(&conversation), 80, 24);
    let row = strip_row(&buffer, &layout);
    assert_eq!(row, "Decisions 1");
    assert!(!row.contains("Agents") && !row.contains("Tasks"), "{row:?}");
}

#[test]
fn no_strip_row_contains_a_driver_wait_string_or_a_control() {
    // Peer-, model- and tool-chosen text (an agent type, a plan title) never reaches the row: a
    // control, an escape sequence or a bidi override in either leaves the row exactly the grammar.
    let hostile = "\u{1b}[31mred\u{1b}]0;title\u{7}\u{202e}evil\u{200b}\u{0}";
    let conversation = conversation(hostile);
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        for host in [Host::Local, Host::Attached] {
            let (mut state, _clock) = clocked(width, height);
            state.task_panel_state.last_executed_plan_id = Some("p1".to_owned());
            mission_bridge::apply_roster(&mut state, vec![agent(hostile, NodeState::Running)]);
            handle_upsert_context_warning(&mut state, "c1", 85);
            state.focus = FocusState::Chat;
            let (buffer, layout) = draw(&mut state, host, Some(&conversation), width, height);
            let row = strip_row(&buffer, &layout);
            let at = format!("{width}x{height} {host:?}: {row:?}");
            for word in DRIVER_WORDS {
                assert!(!row.contains(word), "{at}: {word}");
            }
            assert!(
                row.chars()
                    .all(|c| !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{200b}'))
                    && !row.contains("evil")
                    && !row.contains("red"),
                "{at}"
            );
        }
    }
}

#[tokio::test]
async fn the_local_draw_reserves_the_row_and_the_stage_height_equals_the_paint() {
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        rig.f
            .tabs
            .active_tab_mut()
            .conversation
            .plans
            .insert("p1".to_owned(), plan("Release"));
        rig.f.h.state.task_panel_state.last_executed_plan_id = Some("p1".to_owned());
        rig.paint();
        let layout = rig.f.layout();
        let strip = layout.mission_strip.expect("an empty composer leaves room");
        let at = format!("{width}x{height}");
        let rows = lines(&rig);
        assert_eq!(rows[usize::from(strip.y)].trim_end(), "Tasks 1/3", "{at}");
        assert_eq!(strip.y + 1, layout.status_bar.y, "{at}");
        // Stage height == paint: the viewport the draw recorded is the pane the layout gave.
        assert_eq!(
            rig.f.h.state.viewport_height, layout.chat_pane.height,
            "{at}"
        );
        // The toast capacity follows the new pane height (60x16: 11 rows -> 2 toasts, not 3).
        assert_eq!(
            rustain::adapters::tui::widgets::toast_layer::capacity_for(layout.chat_pane.height),
            if height == 16 { 2 } else { 3 },
            "{at}"
        );
    }
}

#[test]
fn page_keys_skip_no_body_row_at_60x16_whatever_the_composer_height() {
    // A decision frame's body page is smaller than the old fixed `PgDn` step of five rows whenever
    // the Stage is short (the strip row at 60x16, a tall draft): every row must still be on
    // screen at some page, and PgDn/PgUp decide nothing.
    for lines in [0usize, 4, 7, 8] {
        let (mut state, _clock) = clocked(60, 16);
        state.focus = FocusState::Chat;
        state.input_buffer = draft(lines);
        let body: Vec<String> = (0..40).map(|n| format!("row-{n:02}")).collect();
        decision_queue::raise(
            &mut state,
            decision_queue::DecisionDraft::new(
                decision_queue::DecisionKind::LargeImage,
                body.join("\n"),
            ),
            decision_queue::Arrival::Requested,
        );
        let log = facts("c1");
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..80 {
            let buffer = paint(&mut state, &log, 60, 16);
            let text = screen(&buffer);
            seen.extend((0..40).filter(|n| text.contains(&format!("row-{n:02}"))));
            route(
                &mut state,
                &special(rustain::domain::events::DomainKey::PageDown),
                &log,
            );
        }
        assert_eq!(
            seen.len(),
            40,
            "draft={lines}: rows never on screen: {seen:?}"
        );
        assert_eq!(decision_queue::waiting(&state), 1, "paging decides nothing");
    }
}

/// The numbers of the `row-NN` lines on screen: `(first, last)`.
fn visible_rows(text: &str) -> (usize, usize) {
    let numbers: Vec<usize> = text
        .lines()
        .filter_map(|line| {
            let cell = line.trim_matches(|c: char| c == '\u{2502}' || c.is_whitespace());
            cell.strip_prefix("row-")?.parse().ok()
        })
        .collect();
    (
        *numbers.iter().min().expect("a body row is painted"),
        *numbers.iter().max().expect("a body row is painted"),
    )
}

#[test]
fn the_page_step_follows_a_resize_while_scrolled() {
    // A long decision is paged down at a tall terminal, then the terminal shrinks to 60x16 with a
    // tall draft: the next page must start no later than the row after the last one on screen.
    let (mut state, _clock) = clocked(130, 30);
    state.focus = FocusState::Chat;
    let body: Vec<String> = (0..80).map(|n| format!("row-{n:02}")).collect();
    decision_queue::raise(
        &mut state,
        decision_queue::DecisionDraft::new(
            decision_queue::DecisionKind::LargeImage,
            body.join("\n"),
        ),
        decision_queue::Arrival::Requested,
    );
    let log = facts("c1");
    let page_down = |state: &mut rustain::adapters::tui::state::TuiState| {
        route(
            state,
            &special(rustain::domain::events::DomainKey::PageDown),
            &log,
        );
    };
    for _ in 0..2 {
        paint(&mut state, &log, 130, 30);
        page_down(&mut state);
    }
    state.terminal_width = 60;
    state.terminal_height = 16;
    state.input_buffer = draft(7);
    let mut last = visible_rows(&screen(&paint(&mut state, &log, 60, 16))).1;
    for _ in 0..30 {
        page_down(&mut state);
        let (first, end) = visible_rows(&screen(&paint(&mut state, &log, 60, 16)));
        assert!(
            first <= last + 1,
            "a row between {last} and {first} was skipped"
        );
        last = end;
    }
}
