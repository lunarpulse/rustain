//! Frame-owned tool-call inspection; the Stage remains a read-only projection.

pub mod copy;
pub mod layout;
pub mod model;
pub mod project;
pub mod source;

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;

use crate::adapters::tui::{
    presentation::viewer::{RowOrigin, Viewer, ViewerSearch},
    state::{HeightCache, MessageHeightKey},
    theme::Theme,
};
use crate::domain::models::InvocationStatus;

pub use model::{ToolTab, ToolTarget, ToolView};

/// The reducer alone creates/releases entries, including suspended frames.
pub type ToolInspectors = BTreeMap<u64, ToolInspectorState>;

/// Copy payload identity; the reducer allocates the matching frame id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolFrameKey(pub u64);

/// Input can arrive before the first body preparation. Retain its order rather
/// than clamping navigation against a source that has not been rendered yet.
#[derive(Debug, Clone, Copy)]
enum PendingNavigation {
    Key(crate::adapters::tui::presentation::viewer::ViewerCmd),
    Wheel(crate::domain::models::view_state::ScrollDelta),
}

impl PendingNavigation {
    fn apply(self, viewer: &mut Viewer, rows: usize, viewport: usize) {
        use crate::adapters::tui::presentation::viewer;
        match self {
            Self::Key(command) => viewer::apply(viewer, command, rows, viewport),
            Self::Wheel(delta) => viewer::apply_scroll(viewer, delta, rows, viewport),
        }
    }
}

/// Preserve the order of keys across tabs and before the first preparation.
/// Only query-sized drafts are captured; source bodies remain canonical borrows.
#[derive(Debug)]
enum PendingIntent {
    Navigate(ToolTab, bool, PendingNavigation),
    Commit(ToolTab, bool, String),
    Match(ToolTab, bool, bool),
    Tab(ToolTab, bool),
    Wrap(ToolTab, bool),
    Clear,
}

/// Borrow the current owning tab's sources; no inspector snapshots a conversation.
#[derive(Clone, Copy)]
pub struct ToolSources<'a> {
    pub turns: &'a [crate::domain::models::Turn],
    pub open_turn: Option<&'a crate::domain::models::Turn>,
    pub tails: Option<&'a crate::domain::services::reducer::ReducerState>,
}

#[derive(Debug)]
pub struct ToolInspectorState {
    pub target: ToolTarget,
    pub tab: ToolTab,
    pub tabs: &'static [ToolTab],
    pub viewers: [Viewer; 4],
    pub wrap: bool,
    pub viewport: usize,
    pub missing: bool,
    pub status: InvocationStatus,
    pub previous_call: bool,
    pub next_call: bool,
    pub heights: HeightCache,
    pub bodies: [Option<(MessageHeightKey, Vec<Line<'static>>)>; 4],
    pub origins: [Vec<Option<RowOrigin>>; 4],
    pub row_widths: [Vec<usize>; 4],
    pub search: ViewerSearch,
    revisions: [Option<(u64, u16, u64, bool)>; 4],
    search_projection: Option<(ToolTab, u64, u16, bool)>,
    tabs_revision: Option<u64>,
    pending: Vec<PendingIntent>,
}

impl ToolInspectorState {
    pub fn new(target: ToolTarget, viewport: usize) -> Self {
        Self {
            target,
            tab: ToolTab::Output,
            tabs: &[ToolTab::Output, ToolTab::Input, ToolTab::Raw],
            viewers: [Viewer::default(); 4],
            wrap: true,
            viewport,
            missing: false,
            status: InvocationStatus::Pending,
            previous_call: false,
            next_call: false,
            heights: HeightCache::default(),
            bodies: std::array::from_fn(|_| None),
            origins: std::array::from_fn(|_| Vec::new()),
            row_widths: std::array::from_fn(|_| Vec::new()),
            search: ViewerSearch::default(),
            revisions: [None; 4],
            search_projection: None,
            tabs_revision: None,
            pending: Vec::new(),
        }
    }

    pub fn rows(&self) -> &[Line<'static>] {
        self.bodies[self.tab.index()]
            .as_ref()
            .map_or(&[], |(_, rows)| rows)
    }

    fn prepare(
        &mut self,
        inputs: ToolSources<'_>,
        width: u16,
        viewport: usize,
        theme: &Theme,
        now_ms: i64,
    ) {
        let final_tab = self.tab;
        let final_wrap = self.wrap;
        let mut pending = std::mem::take(&mut self.pending);
        for mut intent in pending.drain(..) {
            let (tab, wrap) = match &intent {
                PendingIntent::Navigate(tab, wrap, _)
                | PendingIntent::Commit(tab, wrap, _)
                | PendingIntent::Match(tab, wrap, _)
                | PendingIntent::Tab(tab, wrap) => (*tab, *wrap),
                PendingIntent::Wrap(tab, wrap) => {
                    // Even a toggle before first paint needs the old rows: an
                    // earlier navigation may have landed in a continuation.
                    self.tab = *tab;
                    self.wrap = !*wrap;
                    self.prepare_tab(inputs, width, viewport, theme, now_ms);
                    (*tab, *wrap)
                }
                PendingIntent::Clear => {
                    self.search.commit(String::new());
                    self.search_projection = None;
                    continue;
                }
            };
            self.tab = tab;
            self.wrap = wrap;
            if let PendingIntent::Commit(_, _, query) = &mut intent {
                self.search.commit(std::mem::take(query));
                self.search_projection = None;
            }
            self.prepare_tab(inputs, width, viewport, theme, now_ms);
            if self.missing {
                continue;
            }
            let index = self.tab.index();
            let rows = self.rows().len();
            match intent {
                PendingIntent::Navigate(_, _, action) => {
                    action.apply(&mut self.viewers[index], rows, viewport);
                }
                PendingIntent::Commit(_, _, _) => {
                    self.search.jump(&mut self.viewers[index], rows, viewport);
                }
                PendingIntent::Match(_, _, forward) => {
                    self.search
                        .step(&mut self.viewers[index], forward, rows, viewport);
                }
                PendingIntent::Tab(_, _) | PendingIntent::Wrap(_, _) | PendingIntent::Clear => {}
            }
        }
        self.pending = pending;
        self.tab = final_tab;
        self.wrap = final_wrap;
        self.prepare_tab(inputs, width, viewport, theme, now_ms);
    }

    fn prepare_tab(
        &mut self,
        inputs: ToolSources<'_>,
        width: u16,
        viewport: usize,
        theme: &Theme,
        now_ms: i64,
    ) {
        let view = if let Some(tails) = inputs.tails {
            project::project(inputs.turns, inputs.open_turn, tails, &self.target)
        } else {
            project::without_tail(inputs.turns, inputs.open_turn, &self.target)
        };
        let Some(view) = view else {
            self.missing = true;
            return;
        };
        self.missing = false;
        self.status = view.status.clone();
        if self.tabs_revision != Some(view.source_revision) {
            self.tabs = source::available_tabs(&view);
            let neighbours = project::neighbours(inputs.turns, inputs.open_turn, &self.target);
            self.previous_call = neighbours.as_ref().is_some_and(|n| n.previous.is_some());
            self.next_call = neighbours.as_ref().is_some_and(|n| n.next.is_some());
            self.tabs_revision = Some(view.source_revision);
        }
        if !self.tabs.contains(&self.tab) {
            self.tab = ToolTab::Output;
        }
        self.viewport = viewport;
        let index = self.tab.index();
        let mut revision = source::revision(&view, self.tab);
        if self.tab == ToolTab::Output
            && view.is_running()
            && view.result.is_none()
            && view.tail.is_none_or(str::is_empty)
        {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            revision.hash(&mut hash);
            (now_ms.saturating_sub(view.started_at).max(0) / 100).hash(&mut hash);
            revision = hash.finish();
        }
        let mut styles = std::collections::hash_map::DefaultHasher::new();
        (
            theme.colors.fg_primary,
            theme.colors.fg_secondary,
            theme.colors.fg_muted,
            theme.colors.success,
            theme.colors.error,
        )
            .hash(&mut styles);
        let style_hash = styles.finish();
        // Literal renders unwrapped bodies at usize::MAX, independent of the
        // terminal width. Viewport clamping below still uses the real geometry.
        let width = if self.wrap { width } else { 0 };
        let project_search = !self.search.query().is_empty()
            && self.search_projection != Some((self.tab, revision, width, self.wrap));
        if self.revisions[index] != Some((revision, width, style_hash, self.wrap)) || project_search
        {
            let prepared = source::prepare(&view, self.tab, now_ms);
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            prepared.hash().hash(&mut hash);
            self.wrap.hash(&mut hash);
            style_hash.hash(&mut hash);
            let content_hash = hash.finish();
            let cached = self.bodies[index].as_ref().is_some_and(|(key, _)| {
                key.terminal_width == width && key.content_hash == content_hash
            });
            if !cached {
                let key = MessageHeightKey {
                    msg_id: format!("{}/{}", self.target.tool_call_id, self.tab.label()),
                    terminal_width: width,
                    content_hash,
                };
                let anchor = self.revisions[index]
                    .filter(|(_, old_width, _, wrap)| {
                        *wrap != self.wrap || (self.wrap && *old_width != width)
                    })
                    .and_then(|_| self.origins[index].get(self.viewers[index].top))
                    .and_then(Option::as_ref)
                    .map(|origin| origin.logical_line);
                let rendered = prepared.render(
                    if self.wrap {
                        usize::from(width)
                    } else {
                        usize::MAX
                    },
                    theme,
                );
                if let Some(logical_line) = anchor {
                    self.viewers[index].top = rendered
                        .origins
                        .iter()
                        .position(|origin| {
                            origin
                                .as_ref()
                                .is_some_and(|origin| origin.logical_line >= logical_line)
                        })
                        .unwrap_or_else(|| rendered.lines.len().saturating_sub(1));
                }
                self.row_widths[index] = rendered.lines.iter().map(Line::width).collect();
                if let Some((previous, _)) = self.bodies[index].take() {
                    self.heights.message_entries.remove(&previous);
                }
                self.heights.set_message(key.clone(), rendered.lines.len());
                self.bodies[index] = Some((key, rendered.lines));
                self.origins[index] = rendered.origins;
            }
            if project_search {
                let rows = self.bodies[index]
                    .as_ref()
                    .map_or(0, |(_, rows)| rows.len());
                let maximum = rows.saturating_sub(viewport);
                let top = if self.viewers[index].follow {
                    maximum
                } else {
                    self.viewers[index].top.min(maximum)
                };
                self.search
                    .project(prepared.logical_text(), &self.origins[index], top);
                self.search_projection = Some((self.tab, revision, width, self.wrap));
            }
            self.revisions[index] = Some((revision, width, style_hash, self.wrap));
        }
        let maximum = self.rows().len().saturating_sub(viewport);
        self.viewers[index].top = if self.viewers[index].follow {
            maximum
        } else {
            self.viewers[index].top.min(maximum)
        };
    }
}

impl ToolInspectorState {
    fn navigate(&mut self, action: PendingNavigation) {
        use crate::adapters::tui::presentation::viewer::ViewerCmd;
        use crate::domain::models::view_state::ScrollDelta;
        // Pause a painted tail at the inspected row immediately, so growth
        // arriving before preparation cannot move the navigation's starting row.
        // A never-painted tab must first resolve its honest initial tail.
        if self.bodies[self.tab.index()].is_some()
            && !matches!(
                action,
                PendingNavigation::Key(ViewerCmd::Tail)
                    | PendingNavigation::Wheel(ScrollDelta::Bottom)
            )
        {
            self.viewers[self.tab.index()].follow = false;
        }
        self.pending
            .push(PendingIntent::Navigate(self.tab, self.wrap, action));
    }

    pub(crate) fn accept_search(&mut self) {
        self.search.editor.accept();
        self.pending.push(PendingIntent::Commit(
            self.tab,
            self.wrap,
            self.search.editor.query.as_str().to_owned(),
        ));
    }

    pub(crate) fn clear_search(&mut self) {
        // End text entry now, but clear the committed search in event order:
        // an earlier queued n/N still navigates the old matches.
        self.search.editor.back();
        self.pending.push(PendingIntent::Clear);
    }
}

/// Discovery facts use the live conversation, never a stale copy selection.
pub fn stage_target(
    state: &crate::adapters::tui::state::TuiState,
    conversation: &crate::domain::models::Conversation,
    open_turn: Option<&crate::domain::models::Turn>,
) -> bool {
    if state.focus != crate::domain::models::FocusState::Chat || !state.modal_stack.is_empty() {
        return false;
    }
    let Some(focused) = state.focused_item_id.as_deref() else {
        return false;
    };
    project::contains_call(&conversation.turns, open_turn, focused)
}

/// Event-loop opening seam. Selection changes only after the exact target resolves.
pub fn inspect_focused(
    state: &mut crate::adapters::tui::state::TuiState,
    conversation: &crate::domain::models::Conversation,
    open_turn: Option<&crate::domain::models::Turn>,
) -> bool {
    let Some(id) = state.focused_item_id.as_deref() else {
        return false;
    };
    let target = ToolTarget {
        conversation_id: conversation.id.clone(),
        tool_call_id: id.to_owned(),
    };
    let Some(view) = project::without_tail(&conversation.turns, open_turn, &target) else {
        return false;
    };
    let following = view.is_running();
    let tabs = source::available_tabs(&view);
    let fresh = !state
        .tool_inspectors
        .values()
        .any(|tool| tool.target == target);
    let title = tool_title(&view);
    let viewport =
        frame_rect(state, 1).map_or(1, |rect| usize::from(layout::parts(rect).body.height));
    let selected = target.tool_call_id.clone();
    crate::adapters::tui::presentation::reducer::open_tool(state, target, title, viewport);
    if fresh {
        let id = state
            .modal_stack
            .top()
            .expect("opening seats the tool frame")
            .id;
        let inspector = state
            .tool_inspectors
            .get_mut(&id)
            .expect("frame owns its tool payload");
        inspector.tabs = tabs;
        inspector.viewers[ToolTab::Output.index()].follow = following;
    }
    state.selected_item_id = Some(selected);
    true
}

fn tool_title(view: &ToolView<'_>) -> String {
    let name = crate::adapters::tui::widgets::tool_block::display_tool_name(view.name);
    let safe = crate::domain::services::peer_text::sanitize_tool_text_block(&name);
    let name = if safe.contains('\n') {
        std::borrow::Cow::Owned(safe.replace('\n', " "))
    } else {
        safe
    };
    format!("Tool · {name} #{}", view.ordinal)
}

/// Resolve the key against fresh canonical parts, including pre-paint appends.
pub fn step(
    state: &mut crate::adapters::tui::state::TuiState,
    conversation: &crate::domain::models::Conversation,
    open_turn: Option<&crate::domain::models::Turn>,
    forward: bool,
) -> bool {
    use crate::adapters::tui::presentation::{Surface, reducer};
    let Some(Surface::ToolInspector(ToolFrameKey(id))) =
        state.modal_stack.top().map(|frame| frame.surface)
    else {
        return false;
    };
    let current = &state.tool_inspectors[&id];
    if current.target.conversation_id != conversation.id {
        return false;
    }
    let Some(neighbours) = project::neighbours(&conversation.turns, open_turn, &current.target)
    else {
        return false;
    };
    let Some(next) = (if forward {
        neighbours.next
    } else {
        neighbours.previous
    }) else {
        return false;
    };
    let target = ToolTarget {
        conversation_id: conversation.id.clone(),
        tool_call_id: crate::domain::models::tool_call_id_for(next.turn_id, next.part_id),
    };
    let view = project::without_tail(&conversation.turns, open_turn, &target)
        .expect("the neighbouring canonical invocation exists");
    let title = tool_title(&view);
    let tab = current.tab;
    let viewport = current.viewport;
    let following = view.is_running();
    let tabs = source::available_tabs(&view);
    let mut replacement = ToolInspectorState::new(target, viewport);
    replacement.tabs = tabs;
    replacement.wrap = current.wrap;
    replacement.tab = if tabs.contains(&tab) {
        tab
    } else {
        ToolTab::Output
    };
    replacement.viewers[ToolTab::Output.index()].follow = following;
    reducer::retarget_tool(state, id, replacement, title)
}

pub(crate) fn frame_rect(
    state: &crate::adapters::tui::state::TuiState,
    tab_count: usize,
) -> Option<ratatui::layout::Rect> {
    let area = ratatui::layout::Rect::new(0, 0, state.terminal_width, state.terminal_height);
    let shell = crate::adapters::tui::layout::compute_layout(
        area,
        &state.theme,
        &state.input_buffer,
        tab_count,
        state.sidebar_visible,
        state.density_mode,
    )?;
    Some(crate::adapters::tui::presentation::geometry::frame_rect(
        crate::adapters::tui::presentation::geometry::modal_region(&shell, area),
    ))
}

/// Called before the real draw, with the actual owning tab's reducer and clock.
pub fn refresh(
    state: &mut crate::adapters::tui::state::TuiState,
    conversation: &crate::domain::models::Conversation,
    reducer: Option<&crate::domain::services::reducer::ReducerState>,
    clock: &dyn crate::domain::clock::Clock,
    tab_count: usize,
) {
    if state.tool_inspectors.is_empty() {
        return;
    }
    let Some(rect) = frame_rect(state, tab_count) else {
        return;
    };
    let body = layout::parts(rect).body;
    let inputs = ToolSources {
        turns: &conversation.turns,
        open_turn: reducer.and_then(|reducer| reducer.open_turn.as_ref()),
        tails: reducer,
    };
    let now_ms = clock.wall_now_ms();
    for inspector in state.tool_inspectors.values_mut() {
        if inspector.target.conversation_id != conversation.id {
            inspector.missing = true;
            continue;
        }
        inspector.prepare(
            inputs,
            body.width,
            usize::from(body.height),
            &state.theme,
            now_ms,
        );
    }
}

/// Top-frame keys resolve only in S-TOOL's registered context.
pub fn on_key(
    state: &mut crate::adapters::tui::state::TuiState,
    key: crate::adapters::tui::keymap::Key,
    host: crate::adapters::tui::keymap::Host,
) -> crate::adapters::tui::app::InputAction {
    use crate::adapters::tui::{
        app::InputAction,
        keymap::{self, CommandId, Context, Resolved, table_tool::ToolCmd},
        presentation::{Surface, reducer},
    };
    let Some(Surface::ToolInspector(ToolFrameKey(id))) =
        state.modal_stack.top().map(|frame| frame.surface)
    else {
        return InputAction::Consumed;
    };
    let Resolved::Hit(row) = keymap::resolve(Context::ToolInspector, host, &[key]) else {
        return InputAction::Consumed;
    };
    if row.command == CommandId::Tool(ToolCmd::Back) {
        reducer::back(state);
        return InputAction::Consumed;
    }
    let inspector = state
        .tool_inspectors
        .get_mut(&id)
        .expect("frame owns its tool payload");
    if inspector.missing {
        return InputAction::Consumed;
    }
    match row.command {
        CommandId::Overlay(
            crate::adapters::tui::keymap::table_overlay::OverlayCmd::InspectorSearch,
        ) => {
            inspector.search.editor.open();
        }
        CommandId::Tool(command @ (ToolCmd::NextMatch | ToolCmd::PrevMatch)) => {
            inspector.pending.push(PendingIntent::Match(
                inspector.tab,
                inspector.wrap,
                command == ToolCmd::NextMatch,
            ));
        }
        CommandId::Tool(command @ (ToolCmd::PrevCall | ToolCmd::NextCall)) => {
            return InputAction::StepTool {
                forward: command == ToolCmd::NextCall,
            };
        }
        CommandId::Tool(ToolCmd::Copy) => return InputAction::CopyToolSource,
        CommandId::Tool(ToolCmd::Wrap) => {
            inspector.wrap = !inspector.wrap;
            inspector
                .pending
                .push(PendingIntent::Wrap(inspector.tab, inspector.wrap));
        }
        CommandId::Viewer(command) => {
            inspector.navigate(PendingNavigation::Key(command));
        }
        CommandId::Tool(command @ (ToolCmd::PrevTab | ToolCmd::NextTab)) => {
            let current = inspector
                .tabs
                .iter()
                .position(|tab| *tab == inspector.tab)
                .unwrap_or(0);
            let next = if command == ToolCmd::PrevTab {
                current.saturating_sub(1)
            } else {
                current.saturating_add(1).min(inspector.tabs.len() - 1)
            };
            inspector.tab = inspector.tabs[next];
            inspector
                .pending
                .push(PendingIntent::Tab(inspector.tab, inspector.wrap));
        }
        _ => return InputAction::Consumed,
    }
    state.needs_redraw = true;
    InputAction::Consumed
}

pub fn on_wheel(
    state: &mut crate::adapters::tui::state::TuiState,
    delta: crate::domain::models::view_state::ScrollDelta,
) -> crate::adapters::tui::app::InputAction {
    use crate::adapters::tui::{app::InputAction, presentation::Surface};
    use crate::domain::models::view_state::ScrollDelta;
    let Some(Surface::ToolInspector(ToolFrameKey(id))) =
        state.modal_stack.top().map(|frame| frame.surface)
    else {
        return InputAction::Consumed;
    };
    let inspector = state
        .tool_inspectors
        .get_mut(&id)
        .expect("frame owns its tool payload");
    if inspector.missing {
        return InputAction::Consumed;
    }
    if matches!(
        delta,
        ScrollDelta::ResumeFollowing | ScrollDelta::UnboundedRows(_)
    ) {
        return InputAction::Consumed;
    }
    inspector.navigate(PendingNavigation::Wheel(delta));
    state.needs_redraw = true;
    InputAction::Consumed
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::adapters::tui::markdown::metrics;
    use crate::domain::models::{PartId, ToolOutput, Turn, TurnPart, WriteDiffState};

    fn fixture() -> (Turn, ToolTarget) {
        let mut turn = Turn::new("model".into(), 0);
        let part = turn.push_part(|id| TurnPart::ToolInvocation {
            id,
            tool: "Bash".into(),
            args: serde_json::json!({"command":"seq 1 100"}),
            status: InvocationStatus::Success,
            started_at: 0,
            ended_at: Some(1),
        });
        turn.push_part(|id| TurnPart::ToolResult {
            id,
            refs: part,
            output: ToolOutput {
                content: (0..100).map(|n| format!("row {n}\n")).collect(),
                is_error: false,
                diff: WriteDiffState::NotAWrite,
                images: Vec::new(),
            },
        });
        let target = ToolTarget {
            conversation_id: "conversation".into(),
            tool_call_id: crate::domain::models::turn::tool_call_id_for(&turn.id, PartId(0)),
        };
        (turn, target)
    }

    #[test]
    fn story21_5_body_cache_renders_once_per_tab_key() {
        let (turn, target) = fixture();
        let mut state = ToolInspectorState::new(target.clone(), 12);
        let inputs = ToolSources {
            turns: std::slice::from_ref(&turn),
            open_turn: None,
            tails: None,
        };
        let theme = Theme::dark();
        metrics::reset();
        for _ in 0..5 {
            state.prepare(inputs, 76, 12, &theme, 0);
        }
        assert_eq!(metrics::renders(), 1);
        state.tab = ToolTab::Input;
        state.prepare(inputs, 76, 12, &theme, 0);
        state.tab = ToolTab::Output;
        state.prepare(inputs, 76, 12, &theme, 0);
        assert_eq!(metrics::renders(), 2);
        assert!(state.heights.entries.is_empty());
        assert_eq!(state.heights.message_entries.len(), 2);
        assert_eq!(
            state.rows().len(),
            101,
            "shared Literal appends its paragraph separator"
        );
        assert_eq!(state.rows().last().unwrap().width(), 0);
        state.prepare(inputs, 40, 12, &theme, 0);
        assert_eq!(metrics::renders(), 3);
        assert_eq!(
            state.heights.message_entries.len(),
            2,
            "bounded to one key per tab"
        );
    }
    #[test]
    fn story21_5_unwrapped_resize_reuses_render_and_search_projection() {
        let (turn, target) = fixture();
        let mut state = ToolInspectorState::new(target, 12);
        let inputs = ToolSources {
            turns: std::slice::from_ref(&turn),
            open_turn: None,
            tails: None,
        };
        let theme = Theme::dark();
        state.wrap = false;
        state.search.editor.query.insert("row");
        state.accept_search();
        metrics::reset();
        for width in [76, 40, 100] {
            state.prepare(inputs, width, 12, &theme, 0);
        }
        assert_eq!(metrics::renders(), 1);
        assert!(!state.search.matches.is_empty());
    }

    #[test]
    fn story21_5_wrapped_resize_anchors_continuation_to_logical_line() {
        let (mut turn, target) = fixture();
        let TurnPart::ToolResult { output, .. } = &mut turn.parts[1] else {
            panic!("fixture has a result");
        };
        output.content = (0..40)
            .map(|n| format!("line{n:02}:{}", "x".repeat(193)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = ToolInspectorState::new(target, 12);
        let inputs = ToolSources {
            turns: std::slice::from_ref(&turn),
            open_turn: None,
            tails: None,
        };
        let theme = Theme::dark();
        state.prepare(inputs, 80, 12, &theme, 0);
        state.viewers[0].top = 13;
        state.viewers[0].follow = false;
        assert_eq!(state.origins[0][13].as_ref().unwrap().logical_line, 4);
        state.prepare(inputs, 40, 12, &theme, 0);
        let top = state.viewers[0].top;
        assert_eq!(state.origins[0][top].as_ref().unwrap().logical_line, 4);
        assert_eq!(state.origins[0][top].as_ref().unwrap().bytes.start, 0);
    }

    #[test]
    fn story21_5_search_commit_does_not_pause_before_projection() {
        let (turn, target) = fixture();
        let mut state = ToolInspectorState::new(target, 12);
        let inputs = ToolSources {
            turns: std::slice::from_ref(&turn),
            open_turn: None,
            tails: None,
        };
        let theme = Theme::dark();
        state.viewers[0].follow = true;
        state.prepare(inputs, 76, 12, &theme, 0);
        state.search.editor.query.insert("not present");
        state.accept_search();
        assert!(state.viewers[0].follow);
        state.prepare(inputs, 76, 12, &theme, 0);
        assert!(state.viewers[0].follow);
    }
}
