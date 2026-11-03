//! Frame identities. Only the presentation reducer mutates the stack.

/// The migrated frame inventory; routed popups never consume stack depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    CommandPalette,
    Help,
    NotificationLog,
    QuitConfirm,
    DecisionPane,
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
        }
    }

    /// Drill-downs replace at the cap; the other classes retain one return frame.
    pub(super) const fn keeps_continuation(self) -> bool {
        !matches!(self, Self::CommandPalette)
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
