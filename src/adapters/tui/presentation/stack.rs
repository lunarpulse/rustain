//! Frame identities. Only the presentation reducer mutates the stack.

/// The migrated frame inventory; routed popups never consume stack depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    CommandPalette,
    Help,
    NotificationLog,
    QuitConfirm,
    Decision,
    /// The Needs-you list (S-QUEUE): L3, selects and opens items, never decides.
    DecisionQueue,
    ModelSelector,
    ProfileSwitcher,
    UsagePanel,
    Login,
    Search,
    BookmarkList,
    WaveOverlay,
    CrossSearch,
    ToolInspector(crate::adapters::tui::tool_inspector::ToolFrameKey),
    BlockViewer(crate::adapters::tui::block_viewer::BlockFrameKey),
    /// The A2A thread inspector (S-THREAD, Story 21.8): global, one at a time; its payload
    /// is `TuiState.thread_inspector` and its painted title is the frame's `tool_title`.
    ThreadInspector,
    /// The Mission Control hub (S-MC, Story 21.7): global, one at a time; its payload is
    /// `TuiState.mission_control` (the tab, the selection ids) and its painted title is the
    /// frame's `tool_title` (`Mission Control · <Tab>`).
    MissionControl,
    /// The artifact window (Story 21.7 A15): global, one at a time; its payload is
    /// `TuiState.artifact_window` and its painted title is the frame's `tool_title`
    /// (`Artifact · <name>`).
    ArtifactWindow,
    /// The agent window (Story 21.7 A7): global, one at a time; its payload is
    /// `TuiState.agent_window` and its painted title is the frame's `tool_title`
    /// (`Agent · <type>`).
    AgentWindow,
    /// The task window (Story 21.7 A8): global slot, one at a time, **stage-bound** to the
    /// conversation of its task (the frame carries that conversation id, so a reset, close or
    /// tab switch of that Stage pops it); its payload is `TuiState.task_window` and its
    /// painted title is the frame's `tool_title` (`Task <n> · <title>`).
    TaskWindow,
    /// The Adapters window (Story 21.7 A21): global, one at a time; its payload is
    /// `TuiState.adapters_window` (a snapshot of the port and MCP health taken at open).
    Adapters,
    /// The History window (Story 21.7 A21): global, one at a time; its payload is
    /// `TuiState.history_window` (a snapshot of the session index, re-read when the index
    /// changed). Local only: the attached host has no session index.
    History,
}

impl Surface {
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::CommandPalette => "Command Palette",
            Self::Help => "Help — Rustain Keybindings",
            Self::NotificationLog => "System · Notifications",
            Self::QuitConfirm => "System · Quit",
            Self::Decision => "Decision",
            Self::DecisionQueue => "Needs you",
            Self::ModelSelector => "Select Model",
            Self::ProfileSwitcher => "Profile Switcher",
            Self::UsagePanel => "Usage & Cost",
            Self::Login => "Sign in",
            Self::Search => "Search",
            Self::BookmarkList => "Bookmarks",
            Self::WaveOverlay => "Wave",
            Self::CrossSearch => "Cross-Search",
            Self::ToolInspector(_) => "Tool",
            Self::BlockViewer(_) => "Block",
            Self::ThreadInspector => "Thread",
            Self::MissionControl => "Mission Control",
            Self::ArtifactWindow => "Artifact",
            Self::AgentWindow => "Agent",
            Self::TaskWindow => "Task",
            Self::Adapters => "Adapters",
            Self::History => "History",
        }
    }

    pub const fn context(self) -> crate::adapters::tui::keymap::Context {
        use crate::adapters::tui::keymap::Context;
        match self {
            Self::CommandPalette => Context::CommandPalette,
            Self::Help => Context::Help,
            Self::NotificationLog => Context::NotificationLog,
            Self::QuitConfirm => Context::QuitConfirm,
            Self::Decision => Context::Decision,
            Self::DecisionQueue => Context::DecisionQueue,
            Self::ModelSelector => Context::ModelSelector,
            Self::ProfileSwitcher => Context::ProfileSwitcher,
            Self::UsagePanel => Context::UsagePanel,
            Self::Login => Context::Login,
            Self::Search => Context::SearchTyping,
            Self::BookmarkList => Context::BookmarkList,
            Self::WaveOverlay => Context::WaveOverlay,
            Self::CrossSearch => Context::CrossSearch,
            Self::ToolInspector(_) => Context::ToolInspector,
            Self::BlockViewer(_) => Context::BlockViewer,
            Self::ThreadInspector => Context::ThreadInspector,
            Self::MissionControl => Context::MissionControl,
            Self::ArtifactWindow => Context::ArtifactWindow,
            Self::AgentWindow => Context::AgentWindow,
            Self::TaskWindow => Context::TaskWindow,
            Self::Adapters => Context::Adapters,
            Self::History => Context::History,
        }
    }

    /// Drill-downs replace at the cap; the other classes retain one return frame.
    pub(super) const fn keeps_continuation(self) -> bool {
        !matches!(
            self,
            Self::CommandPalette
                | Self::ModelSelector
                | Self::ProfileSwitcher
                | Self::UsagePanel
                | Self::Search
                | Self::BookmarkList
                | Self::WaveOverlay
                | Self::CrossSearch
                | Self::ToolInspector(_)
                | Self::BlockViewer(_)
                | Self::ThreadInspector
                | Self::MissionControl
                | Self::ArtifactWindow
                | Self::AgentWindow
                | Self::TaskWindow
                | Self::Adapters
                | Self::History
        )
    }
}

/// Stable identity of one opening, independent of its editable state slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: u64,
    pub surface: Surface,
    pub conversation_id: Option<String>,
    /// Queue item identity belongs to the frame, never to the queue payload.
    pub decision: Option<crate::adapters::tui::decision_queue::DecisionKey>,
    /// Display metadata belongs to the frame, not its cached body payload.
    pub tool_title: Option<String>,
    /// One-shot payload preparation after an effect opens a fresh frame.
    /// This is not presence: only membership in the stack owns visibility.
    pub(super) needs_initialization: bool,
}

impl Frame {
    pub fn title(&self) -> &str {
        self.tool_title.as_deref().unwrap_or_else(|| {
            self.decision
                .as_ref()
                .map_or(self.surface.title(), |key| key.title())
        })
    }
}

#[derive(Debug)]
pub(super) struct Continuation {
    pub owner: Surface,
    pub frame: Frame,
}
/// Read-only to callers; frame insertion/removal belongs to the reducer.
#[derive(Debug, Default)]
pub struct ModalStack {
    pub(super) frames: Vec<Frame>,
    pub(super) last_id: u64,
    pub(super) continuations: Vec<Continuation>,
}

impl ModalStack {
    #[must_use]
    pub fn top(&self) -> Option<&Frame> {
        self.frames.last()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Frame> {
        self.frames.iter()
    }

    pub(crate) fn all_frames(&self) -> impl Iterator<Item = &Frame> {
        self.frames
            .iter()
            .chain(self.continuations.iter().map(|c| &c.frame))
    }

    pub(crate) fn all_frames_mut(&mut self) -> impl Iterator<Item = &mut Frame> {
        self.frames
            .iter_mut()
            .chain(self.continuations.iter_mut().map(|c| &mut c.frame))
    }

    /// The owned Login frame's one-shot initialization debt, including continuations.
    pub fn login_initialization(&self) -> Option<bool> {
        self.all_frames()
            .find(|frame| frame.surface == Surface::Login)
            .map(|frame| frame.needs_initialization)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Reopening a covered surface closes the frames above it without resetting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    Pushed,
    Unchanged,
    Truncated,
    Replaced,
    Continued,
}
