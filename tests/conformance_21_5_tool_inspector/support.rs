//! The owning manager, decoder, runtime renderer and compositor are the witnesses.
use crate::e2e_harness::TestHarness;
use ratatui::{buffer::Buffer, layout::Rect};
use rustain::adapters::tui::{
    app::InputAction,
    keymap::{Host, Key},
    layout::{self, AppLayout},
    notification_log::LogFacts,
    presentation::{self, Shell},
    tool_inspector::{self, ToolInspectorState},
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    AnchorMode, Conversation, FocusState, InvocationStatus, PartId, TabManager, ToolOutput, Turn,
    TurnPart, WriteDiffState,
};
use rustain::infrastructure::runtime::event_loop;
use serde_json::Value;
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;

pub fn call(
    turn: &mut Turn,
    name: &str,
    args: Value,
    status: InvocationStatus,
    result: Option<(&str, WriteDiffState)>,
) -> String {
    let failed = status == InvocationStatus::Error;
    let ended_at = (!matches!(
        status,
        InvocationStatus::Pending | InvocationStatus::Running
    ))
    .then_some(1);
    let part = turn.push_part(|id| TurnPart::ToolInvocation {
        id,
        tool: name.into(),
        args,
        status,
        started_at: 0,
        ended_at,
    });
    if let Some((content, diff)) = result {
        turn.push_part(|id| TurnPart::ToolResult {
            id,
            refs: part,
            output: ToolOutput {
                content: content.into(),
                is_error: failed,
                diff,
                images: Vec::new(),
            },
        });
    }
    rustain::domain::models::turn::tool_call_id_for(&turn.id, part)
}

pub fn prose(turn: &mut Turn, text: &str) {
    // Markdown soft breaks reflow: explicit paragraphs make real Stage distance.
    turn.push_part(|id| TurnPart::Prose {
        id,
        text: text.replace('\n', "\n\n"),
    });
}

pub fn rows(count: usize) -> String {
    (0..count)
        .map(|n| format!("row {n:03}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct Fixture {
    pub h: TestHarness,
    pub tabs: TabManager,
    pub clock: Arc<MockClock>,
}

impl Fixture {
    pub fn new(width: u16, height: u16, turn: Turn) -> Self {
        let mut h = TestHarness::with_size(width, height);
        let clock = Arc::new(MockClock::new(Instant::now()));
        h.state.notifications =
            rustain::adapters::tui::notifications::NotificationStore::from_config(
                &rustain::domain::models::NotificationsConfig::default(),
                clock.clone(),
            );
        let mut tabs = TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().clock = clock.clone();
        tabs.active_tab_mut().conversation.turns.push(turn);
        tabs.active_tab_mut().conversation.rebuild_messages_mirror();
        tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
        h.state.active_tab_id = tabs.active_tab_id();
        h.state.focus = FocusState::Chat;
        let mut fixture = Self { h, tabs, clock };
        fixture.load();
        fixture.h.state.focus = FocusState::Chat;
        fixture.paint();
        fixture
    }

    pub fn single(
        width: u16,
        height: u16,
        name: &str,
        args: Value,
        status: InvocationStatus,
        result: Option<(&str, WriteDiffState)>,
    ) -> (Self, String) {
        let mut turn = Turn::new("fixture-model".into(), 0);
        let id = call(&mut turn, name, args, status, result);
        (Self::new(width, height, turn), id)
    }

    pub fn conversation(&self) -> &Conversation {
        &self.tabs.active_tab().conversation
    }

    pub fn load(&mut self) {
        let mut session = self.tabs.active_tab().session.clone();
        let mut queue = self.tabs.active_tab().turn_queue.clone();
        event_loop::load_active_tab(
            &self.tabs,
            &mut self.h.conversation,
            &mut self.h.streaming,
            &mut session,
            &mut self.h.state,
            &mut queue,
        );
    }

    pub fn refresh(&mut self) {
        let before = self
            .h
            .state
            .tab_render_states
            .get(&self.tabs.active_tab_id())
            .map(|_| self.cache());
        let tab = self.tabs.active_tab();
        tool_inspector::refresh(
            &mut self.h.state,
            &tab.conversation,
            Some(&tab.reducer),
            &*tab.clock,
            self.tabs.tabs().len(),
        );
        // The host's draw refreshes block viewers right after tool inspectors.
        rustain::adapters::tui::block_viewer::refresh(
            &mut self.h.state,
            &tab.conversation,
            Some(&tab.reducer),
            self.tabs.tabs().len(),
        );
        let after = self
            .h
            .state
            .tab_render_states
            .get(&self.tabs.active_tab_id())
            .map(|_| self.cache());
        assert_eq!(
            after, before,
            "every production refresh must leave both actual Stage cache halves unchanged"
        );
    }

    pub fn paint(&mut self) -> String {
        self.h.conversation = self.tabs.active_tab().conversation.clone();
        self.h.streaming = self.tabs.active_tab().streaming.clone();
        self.refresh();
        let count = self.tabs.tabs().len();
        event_loop::render(
            &mut self.h.terminal,
            &mut self.h.state,
            &self.h.conversation,
            &self.h.streaming,
            "fixture-model",
            "fixture-provider",
            &rustain::adapters::noop::NoOpSecurity,
            count,
            0,
            Some(&mut self.tabs),
            &rustain::domain::services::session_index::SessionIndex::new(),
            128_000,
        )
        .unwrap();
        self.h.screen_text()
    }

    /// Isolate the production frame pipeline from unrelated Stage markdown work.
    pub fn compose(&mut self) {
        self.refresh();
        let layout = self.layout();
        let state = &self.h.state;
        self.h
            .terminal
            .draw(|frame| {
                let shell = Shell::new(frame.area(), &layout);
                presentation::compose(
                    frame,
                    state,
                    &shell,
                    Host::Local,
                    &LogFacts::default(),
                    &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                    |_| {},
                );
            })
            .unwrap();
    }

    pub fn press(&mut self, key: Key) -> InputAction {
        super::keymap_support::press(&mut self.h.state, key)
    }

    pub fn open(&mut self) {
        assert_eq!(
            self.press(Key::Enter),
            InputAction::InspectTool,
            "retain the Task 3 RED: the production decoder must request inspection"
        );
        assert!(
            self.inspect(),
            "the runtime effect resolves a real render-focused call"
        );
        self.paint();
        assert!(presentation::consistent(&self.h.state));
    }

    /// This is the exact one-call effect used by the host, not reducer::open.
    pub fn inspect(&mut self) -> bool {
        let tab = self.tabs.active_tab();
        tool_inspector::inspect_focused(
            &mut self.h.state,
            &tab.conversation,
            tab.reducer.open_turn.as_ref(),
        )
    }

    /// Drive the registry action through the same fresh canonical host effect.
    pub fn step(&mut self, key: Key) -> bool {
        let InputAction::StepTool { forward } = self.press(key) else {
            panic!("the production registry must request a canonical step");
        };
        let tab = self.tabs.active_tab();
        tool_inspector::step(
            &mut self.h.state,
            &tab.conversation,
            tab.reducer.open_turn.as_ref(),
            forward,
        )
    }

    /// Seat focus by actual Stage paint, not by seeding the inspector target.
    pub fn focus_call(&mut self, wanted: &str) {
        self.paint();
        let max = self
            .h
            .state
            .total_content_height
            .saturating_sub(usize::from(self.h.state.viewport_height));
        for offset in (0..=max).rev() {
            self.tabs.active_tab_mut().view_state.scroll_offset = offset;
            self.h.state.set_scroll_offset(offset);
            self.paint();
            if self.h.state.focused_item_id.as_deref() == Some(wanted) {
                return;
            }
        }
        panic!(
            "production Stage never focused {wanted}; screen: {}",
            self.h.screen_text()
        );
    }

    pub fn tool(&self) -> &ToolInspectorState {
        let frame = self.h.state.modal_stack.top().expect("tool frame");
        let presentation::Surface::ToolInspector(key) = frame.surface else {
            panic!("not a tool frame: {:?}", frame.surface);
        };
        &self.h.state.tool_inspectors[&key.0]
    }

    pub fn layout(&self) -> AppLayout {
        layout::compute_layout(
            Rect::new(
                0,
                0,
                self.h.state.terminal_width,
                self.h.state.terminal_height,
            ),
            &self.h.state.theme,
            &self.h.state.input_buffer,
            self.tabs.tabs().len(),
            false,
            rustain::domain::models::visual::DensityMode::Focus,
        )
        .unwrap()
    }

    pub fn frame_rect(&self) -> Rect {
        presentation::geometry::frame_rect(presentation::geometry::modal_region(
            &self.layout(),
            self.h.terminal.backend().buffer().area,
        ))
    }

    pub fn body_rect(&self) -> Rect {
        tool_inspector::layout::parts(self.frame_rect()).body
    }

    pub fn buffer(&self) -> &Buffer {
        self.h.terminal.backend().buffer()
    }

    pub fn body_row(&self, offset: u16) -> String {
        row(self.buffer(), self.body_rect(), self.body_rect().y + offset)
    }

    pub fn top_is(&self, expected: usize) {
        assert_eq!(self.tool().viewers[self.tool().tab.index()].top, expected);
        if expected < 99 {
            assert!(
                self.body_row(0).contains(&format!("row {expected:03}")),
                "the key must move painted cells, not just viewer state: {:?}",
                self.body_row(0)
            );
        }
    }

    pub fn cache(&self) -> (Vec<String>, Vec<String>, usize) {
        let cache = &self.h.state.tab_render_states[&self.tabs.active_tab_id()].height_cache;
        let mut turns = cache
            .entries
            .iter()
            .map(|(k, v)| format!("{k:?}={v:?}"))
            .collect::<Vec<_>>();
        let mut messages = cache
            .message_entries
            .iter()
            .map(|(k, v)| format!("{k:?}={v:?}"))
            .collect::<Vec<_>>();
        turns.sort();
        messages.sort();
        (turns, messages, cache.last_seen_turn_count)
    }

    pub fn view(&self) -> String {
        let v = &self.tabs.active_tab().view_state;
        let mut collapsed = v
            .collapsed
            .iter()
            .map(|(k, v)| format!("{k:?}={v}"))
            .collect::<Vec<_>>();
        collapsed.sort();
        format!(
            "{:?}/{:?}/{}/{:?}/{}/{collapsed:?}",
            v.mode, v.summary_tier, v.scroll_offset, v.focused_turn, v.pending_append_lines
        )
    }

    pub fn invocation_mut(&mut self, id: &str) -> &mut TurnPart {
        let tab = self.tabs.active_tab_mut();
        let turn = match tab.reducer.open_turn.as_mut() {
            Some(turn) => turn,
            None => tab.conversation.turns.last_mut().unwrap(),
        };
        let turn_id = turn.id.clone();
        turn.parts
            .iter_mut()
            .find(|part| {
                matches!(part, TurnPart::ToolInvocation { id: part_id, .. }
            if rustain::domain::models::turn::tool_call_id_for(&turn_id, *part_id) == id)
            })
            .unwrap()
    }
}

pub fn row(buffer: &Buffer, rect: Rect, y: u16) -> String {
    (rect.x..rect.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

pub fn pixels(buffer: &Buffer, rect: Rect) -> Vec<ratatui::buffer::Cell> {
    (rect.y..rect.bottom())
        .flat_map(|y| (rect.x..rect.right()).map(move |x| buffer[(x, y)].clone()))
        .collect()
}

pub fn part_for(turn: &Turn, id: &str) -> PartId {
    turn.parts
        .iter()
        .find_map(|part| match part {
            TurnPart::ToolInvocation { id: part, .. }
                if rustain::domain::models::turn::tool_call_id_for(&turn.id, *part) == id =>
            {
                Some(*part)
            }
            _ => None,
        })
        .unwrap()
}
