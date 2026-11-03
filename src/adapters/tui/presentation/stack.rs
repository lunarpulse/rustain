//! Frame identities. Only the presentation reducer mutates the stack.

/// The migrated frame inventory; routed popups never consume stack depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    CommandPalette,
    Help,
    NotificationLog,
    QuitConfirm,
    DecisionPane,
    ModelSelector,
    ProfileSwitcher,
    UsagePanel,
    Login,
}

impl Surface {
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::CommandPalette => "Command Palette",
            Self::Help => "Help — Rustain Keybindings",
            Self::NotificationLog => "System · Notifications",
            Self::QuitConfirm => "System · Quit",
            Self::DecisionPane => "System · Decisions",
            Self::ModelSelector => "Select Model",
            Self::ProfileSwitcher => "Profile Switcher",
            Self::UsagePanel => "Usage & Cost",
            Self::Login => "Sign in",
        }
    }

    pub const fn context(self) -> crate::adapters::tui::keymap::Context {
        use crate::adapters::tui::keymap::Context;
        match self {
            Self::CommandPalette => Context::CommandPalette,
            Self::Help => Context::Help,
            Self::NotificationLog => Context::NotificationLog,
            Self::QuitConfirm => Context::QuitConfirm,
            Self::DecisionPane => Context::DecisionPane,
            Self::ModelSelector => Context::ModelSelector,
            Self::ProfileSwitcher => Context::ProfileSwitcher,
            Self::UsagePanel => Context::UsagePanel,
            Self::Login => Context::Login,
        }
    }

    /// Drill-downs replace at the cap; the other classes retain one return frame.
    pub(super) const fn keeps_continuation(self) -> bool {
        !matches!(
            self,
            Self::CommandPalette | Self::ModelSelector | Self::ProfileSwitcher | Self::UsagePanel
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
    pub decision: Option<crate::adapters::tui::decision_pane::DecisionKey>,
    /// One-shot payload preparation after an effect opens a fresh frame.
    /// This is not presence: only membership in the stack owns visibility.
    pub(super) needs_initialization: bool,
}

impl Frame {
    pub fn title(&self) -> &'static str {
        self.decision
            .as_ref()
            .map_or(self.surface.title(), |key| key.title())
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
