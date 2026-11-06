//! Frame-owned tool-call inspection; the Stage remains a read-only projection.

pub mod layout;
pub mod model;
pub mod project;
pub mod source;

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;

use crate::adapters::tui::{
    presentation::viewer::Viewer,
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
        use crate::adapters::tui::presentation::viewer::{self, ViewerCmd};
        use crate::domain::models::view_state::ScrollDelta;
        let command = match self {
            Self::Key(command) => Some(command),
            Self::Wheel(ScrollDelta::WheelUp(lines)) => {
                viewer.top = viewer
                    .top
                    .min(rows.saturating_sub(viewport))
                    .saturating_sub(usize::from(lines));
                viewer.follow = false;
                None
            }
            Self::Wheel(ScrollDelta::WheelDown(lines)) => {
                viewer.top = viewer
                    .top
                    .saturating_add(usize::from(lines))
                    .min(rows.saturating_sub(viewport));
                viewer.follow = false;
                None
            }
            Self::Wheel(delta) => match delta {
                ScrollDelta::LineUp => Some(ViewerCmd::LineUp),
                ScrollDelta::LineDown => Some(ViewerCmd::LineDown),
                ScrollDelta::HalfPageUp => Some(ViewerCmd::HalfUp),
                ScrollDelta::HalfPageDown => Some(ViewerCmd::HalfDown),
                ScrollDelta::FullPageUp => Some(ViewerCmd::PageUp),
                ScrollDelta::FullPageDown => Some(ViewerCmd::PageDown),
                ScrollDelta::Top => Some(ViewerCmd::Top),
                ScrollDelta::Bottom => Some(ViewerCmd::Tail),
                ScrollDelta::ResumeFollowing | ScrollDelta::UnboundedRows(_) => None,
                ScrollDelta::WheelUp(_) | ScrollDelta::WheelDown(_) => unreachable!(),
            },
        };
        if let Some(command) = command {
            viewer::apply(viewer, command, rows, viewport);
        }
    }
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
    pub viewport: usize,
    pub missing: bool,
    pub status: InvocationStatus,
    pub heights: HeightCache,
    pub bodies: [Option<(MessageHeightKey, Vec<Line<'static>>)>; 4],
    revisions: [Option<(u64, u16)>; 4],
    tabs_revision: Option<u64>,
    pending_navigation: [Vec<PendingNavigation>; 4],
}

impl ToolInspectorState {
    pub fn new(target: ToolTarget, viewport: usize) -> Self {
        Self {
            target,
            tab: ToolTab::Output,
            tabs: &[ToolTab::Output, ToolTab::Input, ToolTab::Raw],
            viewers: [Viewer::default(); 4],
            viewport,
            missing: false,
            status: InvocationStatus::Pending,
            heights: HeightCache::default(),
            bodies: std::array::from_fn(|_| None),
            revisions: [None; 4],
            tabs_revision: None,
            pending_navigation: std::array::from_fn(|_| Vec::new()),
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
        if self.revisions[index] != Some((revision, width)) {
            let prepared = source::prepare(&view, self.tab, now_ms);
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            prepared.hash().hash(&mut hash);
            true.hash(&mut hash); // Slice 1 always wraps; slice 2 adds the toggle.
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
                let rows = prepared.render(width, theme);
                if let Some((previous, _)) = self.bodies[index].take() {
                    self.heights.message_entries.remove(&previous);
                }
                self.heights.set_message(key.clone(), rows.len());
                self.bodies[index] = Some((key, rows));
            }
            self.revisions[index] = Some((revision, width));
        }
        self.viewers[index].top = self.viewers[index]
            .top
            .min(self.rows().len().saturating_sub(viewport));
        self.apply_pending_navigation();
    }
}

impl ToolInspectorState {
    fn navigate(&mut self, action: PendingNavigation) {
        let index = self.tab.index();
        if self.bodies[index].is_none() {
            self.pending_navigation[index].push(action);
        } else {
            let rows = self.rows().len();
            action.apply(&mut self.viewers[index], rows, self.viewport);
        }
    }

    fn apply_pending_navigation(&mut self) {
        let index = self.tab.index();
        let rows = self.rows().len();
        for action in self.pending_navigation[index].drain(..) {
            action.apply(&mut self.viewers[index], rows, self.viewport);
        }
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
    let Some(focused) = state.focused_tool_id.as_deref() else {
        return false;
    };
    open_turn
        .into_iter()
        .chain(conversation.turns.iter())
        .any(|turn| {
            turn.parts.iter().any(|part| {
                matches!(part,
            crate::domain::models::TurnPart::ToolInvocation { id, .. }
                if crate::domain::models::turn::tool_call_id_for(&turn.id, *id) == focused)
            })
        })
}

/// Event-loop opening seam. Selection changes only after the exact target resolves.
pub fn inspect_focused(
    state: &mut crate::adapters::tui::state::TuiState,
    conversation: &crate::domain::models::Conversation,
    open_turn: Option<&crate::domain::models::Turn>,
) -> bool {
    let Some(id) = state.focused_tool_id.as_deref() else {
        return false;
    };
    let target = ToolTarget {
        conversation_id: conversation.id.clone(),
        tool_call_id: id.to_owned(),
    };
    let Some(view) = project::without_tail(&conversation.turns, open_turn, &target) else {
        return false;
    };
    let name = crate::adapters::tui::widgets::tool_block::display_tool_name(view.name);
    let safe = crate::domain::services::peer_text::sanitize_tool_text_block(&name);
    let name = if safe.contains('\n') {
        std::borrow::Cow::Owned(safe.replace('\n', " "))
    } else {
        safe
    };
    let title = format!("Tool · {name} #{}", view.ordinal);
    let viewport =
        frame_rect(state, 1).map_or(1, |rect| usize::from(layout::parts(rect).body.height));
    let selected = target.tool_call_id.clone();
    crate::adapters::tui::presentation::reducer::open_tool(state, target, title, viewport);
    state.selected_tool_id = Some(selected);
    true
}

fn frame_rect(
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
}
