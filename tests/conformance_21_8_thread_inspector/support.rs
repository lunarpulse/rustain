//! Shared fixtures for the 21-8 thread-window keystones: a real workspace journal that
//! received items are appended to through the production room-journal writer, a `Rig`
//! (the 21-5 `Fixture`: owning manager, runtime renderer, compositor) that opens the
//! window through `/team view` and sends crossterm events through the local host's
//! `handle_operator_input`, and readers of the painted frame. No sleeps.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use rustain::adapters::tui::app::{
    InputAction, convert_crossterm_event, handle_operator_input, submit_message_for_test,
};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::state::{ThreadInspectorState, TuiState};
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    AppConfig, Authorship, ItemAddress, ItemId, JournalRecord, MouseConfig, PeerId,
    ReceivedStories, RoomEvent, SemanticMessageType, SendAuthorization, Turn,
};
use rustain::domain::ports::{RoomJournal, RoomJournalReader, StoragePort, StreamingProvider};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::runtime::transparency_bridge::team_command;
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal};

pub use crate::conformance_21_3_keymap::support::key_event;
pub use crate::conformance_21_5_tool_inspector::support::{Fixture, pixels, prose, rows};

/// The label every seeded item carries (`alias`), so the View's attribution line is exact.
pub const SENDER: &str = "ada";

/// A real workspace journal in a scratch directory (hermetic: nothing outside it).
pub struct Workspace {
    pub dir: tempfile::TempDir,
    pub node: Arc<NodeJournal>,
    pub room: Arc<dyn RoomJournal>,
}

/// One received item to append.
#[derive(Clone)]
pub struct Item {
    pub id: String,
    pub kind: SemanticMessageType,
    pub content: String,
    pub stories: Option<ReceivedStories>,
    pub in_reply_to: Option<String>,
    pub authorship: Authorship,
}

impl Item {
    pub fn new(id: &str, kind: SemanticMessageType, content: &str) -> Self {
        Self {
            id: id.to_owned(),
            kind,
            content: content.to_owned(),
            stories: None,
            in_reply_to: None,
            authorship: Authorship::Unknown,
        }
    }

    pub fn reply_to(mut self, parent: &str) -> Self {
        self.in_reply_to = Some(parent.to_owned());
        self
    }

    pub fn agent_composed(mut self) -> Self {
        self.authorship = Authorship::AgentComposed;
        self
    }

    pub fn stories(mut self, stories: ReceivedStories) -> Self {
        self.stories = Some(stories);
        self
    }
}

pub fn address(item: &str) -> ItemAddress {
    ItemAddress::from_a2a_ingress(
        PeerId::from_public_key(&[7u8; 32]).expect("32-byte key"),
        ItemId::from_replay(item),
    )
}

impl Workspace {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().expect("scratch workspace");
        let node = Arc::new(
            NodeJournal::open_workspace(dir.path())
                .await
                .expect("open the production node journal"),
        );
        let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(node.clone(), None));
        Self { dir, node, room }
    }

    pub async fn with(items: &[Item]) -> Self {
        let workspace = Self::new().await;
        for item in items {
            workspace.receive(item).await;
        }
        workspace
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub async fn receive(&self, item: &Item) {
        self.room
            .record_event(RoomEvent::RecipientItemReceived {
                address: address(&item.id),
                task: format!("task-{}", item.id),
                alias: Some(SENDER.to_owned()),
                message_type: item.kind,
                stories: item.stories.clone(),
                selected_for: None,
                authorship: item.authorship,
                send_authorization: SendAuthorization::Unknown,
                content: item.content.clone(),
                in_reply_to: item.in_reply_to.clone(),
                capability_hash: None,
                reply_capability_matched: false,
            })
            .await
            .expect("append the received item");
    }

    pub async fn acknowledge(&self, id: &str) {
        self.room
            .record_event(RoomEvent::RecipientItemAcknowledged {
                address: address(id),
                alias: Some(SENDER.to_owned()),
            })
            .await
            .expect("append the acknowledgement");
    }

    pub async fn remove(&self, id: &str) {
        self.room
            .record_event(RoomEvent::RecipientItemRemoved {
                address: address(id),
            })
            .await
            .expect("append the removal");
    }

    pub async fn retract(&self, id: &str) {
        self.room
            .record_event(RoomEvent::RecipientItemRetracted {
                address: address(id),
                retracted_at_ms: 1_700_000_000_000,
                principal_collapsed: false,
            })
            .await
            .expect("append the retraction");
    }

    /// Every journal row, room or not (journal-row witness for "nothing was written").
    pub async fn journal_rows(&self) -> usize {
        RoomJournalReader::load_entries(self.node.as_ref())
            .await
            .expect("read the journal")
            .into_iter()
            .filter(|entry| matches!(entry.record, JournalRecord::Room(_)))
            .count()
    }
}

/// The one consultation every simple case opens.
pub const CONSULT_BODY: &str = "Please review the plan\nsecond line";

pub async fn consultation_workspace() -> Workspace {
    Workspace::with(&[
        Item::new("ri_c", SemanticMessageType::Consultation, CONSULT_BODY).agent_composed(),
    ])
    .await
}

/// `count` follow-ups chained from `ri_000`: ids `ri_000 ..= ri_{count-1}`; every body is
/// padded so the rows exceed any test viewport.
pub fn chain(count: usize) -> Vec<Item> {
    (0..count)
        .map(|n| {
            let mut item = Item::new(
                &format!("ri_{n:03}"),
                SemanticMessageType::Consultation,
                &format!("turn {n:03} body {}", "w".repeat(70)),
            );
            if n > 0 {
                item = item.reply_to(&format!("ri_{:03}", n - 1));
            }
            item
        })
        .collect()
}

/// A 60-turn thread whose newest page omits older turns (the 50-turn bound).
pub async fn long_workspace() -> Workspace {
    Workspace::with(&chain(60)).await
}

pub const NEWEST: &str = "ri_059";

pub fn plain_turn() -> Turn {
    let mut turn = Turn::new("fixture-model".into(), 0);
    prose(&mut turn, "Stage answer paragraph.");
    turn
}

pub struct Rig {
    pub f: Fixture,
    pub ws: Workspace,
    pub app: AppState,
    pub events: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
}

impl Rig {
    pub async fn new(ws: Workspace, width: u16, height: u16, turn: Turn) -> Self {
        let (app, events) = app_state(ws.path());
        Self {
            f: Fixture::new(width, height, turn),
            ws,
            app,
            events,
        }
    }

    pub async fn plain(ws: Workspace, width: u16, height: u16) -> Self {
        Self::new(ws, width, height, plain_turn()).await
    }

    /// `/team view <id>`: the command text goes through the production parser on a scratch
    /// composer (so the operator's own draft is not touched), the arguments through the
    /// real `team_command` front door against the live state.
    pub async fn view(&mut self, id: &str) {
        let mut scratch = TuiState::new(130, 30);
        scratch.input_buffer = format!("/team view {id}");
        let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut scratch)
        else {
            panic!("/team view must use the production command parser");
        };
        assert_eq!(name, "team");
        team_command(
            &mut self.f.h.state,
            "recipient-view",
            args.as_deref(),
            &self.app,
        )
        .await;
    }

    /// The page-load effect the event loop runs for `InputAction::ThreadPage`.
    pub async fn navigate(&mut self, earlier: bool) {
        rustain::infrastructure::runtime::thread_open::navigate(
            self.ws.path(),
            None,
            &mut self.f.h.state,
            earlier,
        )
        .await
        .expect("the earlier/newer bounded page loads");
    }

    /// A raw terminal event through the local host's ownership filter and converter.
    pub fn send(&mut self, event: KeyEvent) -> InputAction {
        let raw = Event::Key(event);
        let Some(domain) = convert_crossterm_event(&raw, &MouseConfig::default()) else {
            return InputAction::Ignored;
        };
        handle_operator_input(&mut self.f.h.state, &raw, &domain).unwrap_or(InputAction::Ignored)
    }

    /// A plain Press of `key` exactly as the terminal decodes it.
    pub fn key(&mut self, key: Key) -> InputAction {
        self.send(key_event(key))
    }

    pub fn keys(&mut self, keys: &[Key]) {
        for key in keys {
            self.key(*key);
        }
    }

    pub fn paint(&mut self) {
        self.f.paint();
    }

    /// A real terminal resize: the backend, the state's size (the layout reads it) and the
    /// production `Resize` input all change, so the next paint lays out at the new width.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.f.h.terminal.backend_mut().resize(width, height);
        let raw = Event::Resize(width, height);
        if let Some(domain) = convert_crossterm_event(&raw, &MouseConfig::default()) {
            let _ = handle_operator_input(&mut self.f.h.state, &raw, &domain);
        }
        // The layout reads the state's size; whatever the input path did, it now is the new size.
        self.f.h.state.terminal_width = width;
        self.f.h.state.terminal_height = height;
    }

    /// One wheel notch through the local host's converter and ownership filter.
    pub fn wheel(&mut self, down: bool) -> InputAction {
        let raw = Event::Mouse(crossterm::event::MouseEvent {
            kind: if down {
                crossterm::event::MouseEventKind::ScrollDown
            } else {
                crossterm::event::MouseEventKind::ScrollUp
            },
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        let Some(domain) = convert_crossterm_event(&raw, &MouseConfig::default()) else {
            return InputAction::Ignored;
        };
        handle_operator_input(&mut self.f.h.state, &raw, &domain).unwrap_or(InputAction::Ignored)
    }

    pub fn inspector(&self) -> &ThreadInspectorState {
        self.f
            .h
            .state
            .thread_inspector
            .as_ref()
            .expect("the thread window is open")
    }

    pub fn top(&self) -> Surface {
        self.f
            .h
            .state
            .modal_stack
            .top()
            .expect("a frame is open")
            .surface
    }

    pub fn depth(&self) -> usize {
        self.f.h.state.modal_stack.len()
    }

    pub fn is_open(&self) -> bool {
        self.f.h.state.modal_stack.len() == 1
            && self.top() == Surface::ThreadInspector
            && self.f.h.state.thread_inspector.is_some()
            && presentation::consistent(&self.f.h.state)
    }

    /// `(rows, viewport)` of the last paint.
    pub fn fit(&self) -> (usize, usize) {
        self.inspector().fit.get()
    }

    pub fn viewer_top(&self) -> usize {
        self.inspector().viewer.top
    }

    /// The `SystemNotice` warnings the command emitted so far: `(conversation, message)`.
    pub fn warnings(&mut self) -> Vec<(Option<String>, String)> {
        let mut warnings = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let AppEvent::SystemNotice {
                conversation_id,
                level,
                message,
            } = event
            {
                assert_eq!(
                    level,
                    rustain::domain::models::NoticeLevel::Warning,
                    "every refusal of the local view is a Warning"
                );
                warnings.push((conversation_id, message));
            }
        }
        warnings
    }
}

pub fn raw(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
    KeyEvent::new_with_kind(code, modifiers, kind)
}

pub fn press_char(c: char) -> KeyEvent {
    raw(KeyCode::Char(c), KeyModifiers::NONE, KeyEventKind::Press)
}

/// Every cell of one buffer row from `x0` for `width` cells (wide-glyph continuation
/// cells carry no text of their own).
fn row_text(f: &Fixture, x0: u16, y: u16, width: u16) -> String {
    (x0..x0 + width)
        .map(|x| f.buffer()[(x, y)].symbol())
        .collect()
}

pub fn frame_rect(f: &Fixture) -> Rect {
    f.frame_rect()
}

/// The frame's inner rows (borders excluded), trailing blanks trimmed.
pub fn inner_rows(f: &Fixture) -> Vec<String> {
    let inner = frame_rect(f).inner(Margin::new(1, 1));
    (inner.y..inner.bottom())
        .map(|y| row_text(f, inner.x, y, inner.width).trim_end().to_owned())
        .collect()
}

/// The painted top border row of the frame (the title row).
pub fn title_row(f: &Fixture) -> String {
    let rect = frame_rect(f);
    row_text(f, rect.x, rect.y, rect.width)
}

pub fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Where the painted rows of the window sit, derived from the last paint: pinned rows,
/// the scrolling body viewport and the rail.
pub struct Geometry {
    pub inner: Rect,
    pub header: u16,
    pub body: u16,
    pub footer: u16,
}

pub fn geometry(rig: &Rig) -> Geometry {
    let inner = frame_rect(&rig.f).inner(Margin::new(1, 1));
    let inspector = rig.inspector();
    let cache = inspector.layout.borrow();
    let room = inner.height;
    let body_min = u16::from(room > 0);
    // An open reply field or a filter field takes one row below the body (the widget's plan).
    let field_rows = u16::from(
        inspector.reply.is_some() || inspector.search.editing || !inspector.search.query.is_empty(),
    );
    let field = if room > body_min { field_rows } else { 0 };
    let rail_room = room.saturating_sub(body_min + field);
    let rows_of = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
    let (info_rows, reader_rows) = (
        rows_of(cache.footer_info.len()),
        rows_of(cache.footer_reader.len()),
    );
    // The widget's one fitting plan: the writer rail is painted only when it fits whole with
    // the reader rail inside `room - 1 - field`; otherwise the reader rail is clipped to that room.
    let footer = if info_rows.saturating_add(reader_rows) <= rail_room {
        info_rows + reader_rows
    } else {
        reader_rows.min(rail_room)
    };
    let header = rows_of(cache.header.len()).min(room.saturating_sub(body_min + field + footer));
    let body = room - header - field - footer;
    assert_eq!(
        usize::from(body),
        inspector.fit.get().1,
        "the painted body viewport is the viewport the viewer clamps against"
    );
    Geometry {
        inner,
        header,
        body,
        footer,
    }
}

/// The pinned identity rows, the body viewport rows and the rail rows as painted.
pub fn header_rows(rig: &Rig) -> Vec<String> {
    let g = geometry(rig);
    (0..g.header)
        .map(|n| {
            row_text(&rig.f, g.inner.x, g.inner.y + n, g.inner.width)
                .trim_end()
                .to_owned()
        })
        .collect()
}

pub fn body_rows(rig: &Rig) -> Vec<String> {
    let g = geometry(rig);
    (0..g.body)
        .map(|n| {
            row_text(&rig.f, g.inner.x, g.inner.y + g.header + n, g.inner.width)
                .trim_end()
                .to_owned()
        })
        .collect()
}

pub fn footer_rows(rig: &Rig) -> Vec<String> {
    let g = geometry(rig);
    (0..g.footer)
        .map(|n| {
            row_text(
                &rig.f,
                g.inner.x,
                g.inner.y + g.header + g.body + n,
                g.inner.width,
            )
            .trim_end()
            .to_owned()
        })
        .collect()
}

/// Whether the last paint shows the `[p]` Earlier / `[l]` Newer hints (read from the painted
/// rail cells, not from any state flag).
pub fn painted_pager_hints(rig: &Rig) -> (bool, bool) {
    let rail = footer_rows(rig).join("\n");
    (rail.contains("[p]"), rail.contains("[l]"))
}

/// Every body row the window can show, collected from what is PAINTED while the viewer
/// pages from the top to the end through the real `PageDown` key. The result must have
/// exactly `fit().0` rows: the clamped row count is the painted row count.
pub fn painted_body(rig: &mut Rig) -> Vec<String> {
    rig.key(Key::Char('g'));
    rig.paint();
    let (rows, viewport) = rig.fit();
    let max = rows.saturating_sub(viewport);
    let mut all: Vec<Option<String>> = vec![None; rows];
    for _ in 0..=rows + 2 {
        let top = rig.viewer_top().min(max);
        for (n, row) in body_rows(rig)
            .into_iter()
            .take(rows.saturating_sub(top))
            .enumerate()
        {
            all[top + n] = Some(row);
        }
        if top >= max {
            break;
        }
        rig.key(Key::PageDown);
        rig.paint();
    }
    all.into_iter()
        .enumerate()
        .map(|(n, row)| row.unwrap_or_else(|| panic!("body row {n} of {rows} was never painted")))
        .collect()
}

pub fn app_state(workspace: &Path) -> (AppState, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    use clap::Parser;
    use rustain::adapters::noop::{
        NoOpApprovalPersistence, NoOpMemory, NoOpProvider, NoOpStorage, NoOpUsageLedger,
    };
    use rustain::domain::services::plan_manager::PlanManager;
    use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
    use rustain::infrastructure::composition::ComposeContext;
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    use rustain::infrastructure::runtime::event_bus::EventBus;

    let approval_runtime = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let provider_swap = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpProvider) as Arc<dyn StreamingProvider>
    ));
    let (event_bus, domain_rx) = EventBus::new(64);
    let compose_snapshot = Arc::new(ComposeContext {
        workspace_path: workspace.to_path_buf(),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(NoOpStorage) as Arc<dyn StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpMemory) as Arc<dyn rustain::domain::ports::MemoryPort>
        )),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        peer_topic_store: Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    });
    AppState::new(
        Arc::new(event_bus),
        domain_rx,
        approval_runtime,
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
        Arc::new(PlanManager::new(workspace.to_path_buf())),
        Arc::new(DefaultPlanInjector::new()),
        provider_swap,
        Arc::new(rustain::adapters::provider::ProviderRegistry::new()),
        Arc::new(NoOpUsageLedger),
        Arc::new(rustain::adapters::budget::BudgetStateStore::new()),
        Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        Arc::new(AgentCore::test_noop()),
        None,
        compose_snapshot,
        Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::profile_resolver::noop::NoopProfileResolver,
        )
            as Arc<dyn rustain::domain::ports::ProfileResolver>)),
        rustain::adapters::cli::commands::Cli::try_parse_from(["rustain"]).expect("bare cli"),
        None,
        rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        #[cfg(feature = "meta-search")]
        None,
    )
}
